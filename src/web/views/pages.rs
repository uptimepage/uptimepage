//! Operator UI for managing public status pages: a list (`/settings/pages`)
//! and a per-page editor (`/settings/pages/{id}`). Both drive the
//! `/api/v1/status-pages` JSON API via fetch + `smRenderApiError` (the shared
//! modern-form pattern), so this module only renders chrome + initial state.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{Locale, OrgId, PublicStyle};
use crate::error::AppError;
use crate::public_status::urls::{
    public_base, public_host_suffix, public_logo_url, public_status_url,
};
use crate::request::CurrentUser;
use crate::request::auth::CurrentOrg;
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::resolve_org;

const TAB_PAGES: &str = "pages";

/// Upper bound on monitors offered as curation candidates in the page editor.
/// Matches the store's hard `LIMIT` ceiling, so every monitor an org can own is
/// reachable from the editor.
const MAX_CURATION_CANDIDATES: usize = 10_000;

// ── List ──────────────────────────────────────────────────────────────────────

pub struct PageRow {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub enabled: bool,
    /// Absolute live URL, or empty in path mode.
    pub url: String,
    pub subscriber_count: i64,
    /// The plan no longer covers this page, so its URL 404s and its
    /// subscribers hear nothing. `enabled` is untouched underneath, which is
    /// what it returns to.
    pub plan_held: bool,
    /// Only the people on its access list can open it.
    pub private: bool,
    /// Viewers not blocked, for a private page's card.
    pub viewers: i64,
}

impl PageRow {
    /// What the page is, or returns to once its plan covers it again.
    pub fn visibility(&self) -> &'static str {
        visibility(self.enabled, self.private)
    }
}

/// The three states the rail offers, from the two stored flags.
fn visibility(enabled: bool, private: bool) -> &'static str {
    match (enabled, private) {
        (false, _) => "off",
        (true, true) => "private",
        (true, false) => "public",
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/pages.html")]
pub struct PagesPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/pages_partial.html")]
pub struct PagesPartial {
    pub rows: Vec<PageRow>,
    /// The add card is the last cell of the grid, so the partial carries the
    /// cap too. The API is the authority; this only shapes what is offered.
    pub at_limit: bool,
    /// `None` where the plan sets no real ceiling, so the card says nothing
    /// rather than printing the sentinel as a number.
    pub max_pages: Option<i32>,
}

async fn build_rows(state: &AppState, org: OrgId) -> WebResult<Vec<PageRow>> {
    let pages = state.status_page_store.list(org).await?;
    let counts: std::collections::HashMap<Uuid, i64> = match state.db.as_ref() {
        Some(pool) => crate::storage::subscribers::verified_counts(pool, org.0)
            .await?
            .into_iter()
            .collect(),
        None => std::collections::HashMap::new(),
    };
    let (access, viewers) = match state.db.as_ref() {
        Some(pool) => (
            crate::storage::page_access::settings::load_for_org(pool, org).await?,
            crate::storage::page_access::viewers::counts_for_org(pool, org).await?,
        ),
        None => Default::default(),
    };
    Ok(pages
        .into_iter()
        .map(|p| {
            let url = public_base(&state.cfg, &p.slug)
                .map(|o| public_status_url(&state.cfg, &o))
                .unwrap_or_default();
            let subscriber_count = counts.get(&p.id.0).copied().unwrap_or(0);
            PageRow {
                id: p.id.to_string(),
                private: access.get(&p.id).is_some_and(|a| a.is_private()),
                viewers: viewers.get(&p.id).map_or(0, |c| c.active),
                name: p.name,
                slug: p.slug,
                enabled: p.enabled,
                plan_held: p.plan_hold_at.is_some(),
                url,
                subscriber_count,
            }
        })
        .collect())
}

/// The shell only carries chrome — the grid, its cap, and the help block all
/// come from the partial, so nothing here needs to read the org's pages.
pub async fn pages_list(org: Result<CurrentOrg, AppError>) -> WebResult<Response> {
    if let Err(resp) = resolve_org(org, "/settings/pages") {
        return Ok(*resp);
    }
    Ok(PagesPage {
        active_tab: TAB_PAGES,
    }
    .into_response())
}

pub async fn pages_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/pages") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let rows = build_rows(&state, org).await?;
    // Self-host reports `i32::MAX`, and an override that overflows clamps to
    // it. That exact value is the "no ceiling" sentinel; anything else is a
    // real cap, however large.
    let cap = state.quotas.limit_for_org(org).await?.max_status_pages;
    let max_pages = (cap != i32::MAX).then_some(cap);
    Ok(PagesPartial {
        at_limit: max_pages.is_some_and(|m| rows.len() as i32 >= m),
        max_pages,
        rows,
    }
    .into_response())
}

// ── Editor ────────────────────────────────────────────────────────────────────

/// One monitor as a curation candidate: whether it's on this page, and its
/// per-page overrides when it is.
pub struct ComponentRow {
    pub target_id: String,
    pub monitor_name: String,
    pub kind: &'static str,
    pub name_hint: &'static str,
    pub on_page: bool,
    pub public_name: String,
    pub public_group: String,
    pub detail_link: bool,
    /// The heading of the group section that shows it; it leaves only with
    /// the group.
    pub section: Option<String>,
}

/// A monitor group shown on the page.
pub struct SectionRow {
    pub id: String,
    pub heading: String,
    pub group_name: String,
    pub members: usize,
    pub left_off: u32,
}

/// A group the page does not show yet.
pub struct GroupChoice {
    pub id: String,
    pub name: String,
}

pub(crate) fn kind_label(kind: &str) -> &'static str {
    match kind {
        "http" => "HTTP",
        "tcp" => "TCP",
        "ping" => "Ping",
        "heartbeat" => "Heartbeat",
        "manual" => "Manual",
        "dns" => "DNS",
        "tls_cert" => "TLS",
        "domain_expiry" => "Domain",
        "flow" => "Flow",
        _ => "Check",
    }
}

fn name_hint(kind: &str) -> &'static str {
    match kind {
        "http" => "e.g. Website",
        "tcp" => "e.g. TCP port",
        "ping" => "e.g. Gateway",
        "heartbeat" => "e.g. Nightly backup",
        "manual" => "e.g. SIP trunks",
        "dns" => "e.g. DNS",
        "tls_cert" => "e.g. TLS certificate",
        "domain_expiry" => "e.g. Domain expiry",
        "flow" => "e.g. Login flow",
        _ => "Public display name",
    }
}

pub struct StyleOption {
    pub value: &'static str,
    pub selected: bool,
}

pub struct LocaleOption {
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

/// One subscriber row in the editor's roster. `contact` is masked for email.
pub struct SubscriberView {
    pub id: String,
    pub contact: String,
    pub channel: String,
    pub confirmed: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Made by a visitor signed in with a page password.
    pub via_password: bool,
    with_current_password: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/page_editor.html")]
pub struct PageEditorPage {
    pub active_tab: &'static str,
    pub id: String,
    pub slug: String,
    pub name: String,
    pub enabled: bool,
    pub public_display_name: String,
    pub public_about: String,
    pub brand_color_value: String,
    /// `None` while the page inherits the deployment default.
    pub show_powered_by: Option<bool>,
    pub powered_by_default: bool,
    pub white_label: bool,
    pub hide_from_search: bool,
    pub website_url: String,
    pub styles: Vec<StyleOption>,
    pub locales: Vec<LocaleOption>,
    /// Groups already in use, offered as suggestions so a second spelling of
    /// one group does not quietly become a second section on the page.
    pub groups: Vec<String>,
    pub host_suffix: Option<String>,
    pub status_url: String,
    /// Absolute base for the embeddable badge SVG. `None` only when no public
    /// base URL is configured.
    pub badge_url: Option<String>,
    /// Status-page URL (with UTM) the badge anchor links to, so the embed is a
    /// real backlink, not a bare hotlinked image.
    pub badge_link: Option<String>,
    /// Overall-badge alt text, safe to paste into an HTML `alt="…"`.
    pub badge_alt: String,
    pub logo_url: String,
    /// Intrinsic logo dims for CLS-safe `<img width height>`; 0 = no logo.
    pub logo_w: i64,
    pub logo_h: i64,
    pub max_logo_bytes: u64,
    pub max_logo_dim_px: u32,
    /// Monitors on this page first (in order), then the rest by name.
    pub components: Vec<ComponentRow>,
    pub sections: Vec<SectionRow>,
    pub group_choices: Vec<GroupChoice>,
    /// Confirmed + pending subscribers, newest first.
    pub subscribers: Vec<SubscriberView>,
    /// `public`, `private` or `off`, as the rail's first legend reads.
    pub visibility: &'static str,
    /// `Some` when the plan does not include private pages.
    pub lock: Option<crate::web::views::team_lock::TeamLock>,
    /// The stored access settings and list, for the access tab's script.
    pub access_json: String,
    /// The page password's line, whose `set_at` also says whether one is
    /// stored while the way in is off.
    pub password_json: String,
    /// Subscriptions made with the current password, which going private by
    /// the password alone keeps.
    pub subscribers_with_password: usize,
    pub rule_count: usize,
    pub viewers_active: i64,
    pub viewers_blocked: i64,
}

pub async fn page_editor(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    use crate::domain::StatusPageId;
    let org = match resolve_org(org, "/settings/pages") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let page_id = StatusPageId(id);
    let Some(page) = state.status_page_store.get(org, page_id).await? else {
        return Ok((
            axum::http::StatusCode::NOT_FOUND,
            crate::web::error::NotFoundPage {
                active_tab: TAB_PAGES,
            },
        )
            .into_response());
    };

    let plan = state.quotas.limit_for_org(org).await?;
    let white_label = plan.white_label_enabled;
    let lock = match (plan.private_pages_enabled, state.db.as_ref()) {
        (true, _) => None,
        (false, Some(pool)) => Some(crate::web::views::team_lock::TeamLock {
            payer: crate::storage::accounts::pays_for_org(pool, user, org).await?,
            teaser: true,
        }),
        (false, None) => Some(crate::web::views::team_lock::TeamLock {
            payer: false,
            teaser: true,
        }),
    };
    let page_ref = crate::domain::PageRef { page: page_id, org };
    let (access, counts, password) = match state.db.as_ref() {
        Some(pool) => {
            let access = crate::storage::page_access::settings::load(pool, page_ref)
                .await?
                .unwrap_or_default();
            let password =
                crate::storage::page_access::password::summary(pool, page_ref, &access).await?;
            (
                access,
                crate::storage::page_access::viewers::counts_for_page(pool, org, page_id).await?,
                password,
            )
        }
        None => Default::default(),
    };
    let visibility = visibility(page.enabled, access.is_private());
    let access_json = serde_json::to_string(&crate::domain::AccessSpec::from(&access))
        .map_err(|e| AppError::Other(anyhow::anyhow!("encode page access: {e}")))?;
    let password_json = serde_json::to_string(&password)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encode page password: {e}")))?;

    // On-page components (curated, in order) + every other org monitor.
    let curated = state
        .status_page_store
        .list_components(org, page_id)
        .await?;
    let on_page: std::collections::HashSet<Uuid> = curated.iter().map(|c| c.target_id).collect();
    // Pull every target: the default filter caps at 100, which would silently
    // hide monitors past that from large orgs.
    let all_targets = state
        .target_store
        .list(
            org,
            crate::storage::TargetFilter {
                limit: Some(MAX_CURATION_CANDIDATES),
                ..Default::default()
            },
        )
        .await?;
    let kind_by_id: std::collections::HashMap<Uuid, &'static str> =
        all_targets.iter().map(|t| (t.id, t.check.kind())).collect();
    let (sections, group_choices) = match state.db.as_ref() {
        Some(pool) => {
            let sections = crate::storage::page_sections::list(pool, org, id).await?;
            let shown: std::collections::HashSet<Uuid> =
                sections.iter().map(|s| s.group_id).collect();
            let choices = state
                .monitor_group_store
                .list(org)
                .await?
                .into_iter()
                .filter(|g| !shown.contains(&g.id))
                .map(|g| GroupChoice {
                    id: g.id.to_string(),
                    name: g.name,
                })
                .collect();
            (sections, choices)
        }
        None => (Vec::new(), Vec::new()),
    };
    let heading_of: std::collections::HashMap<Uuid, String> = sections
        .iter()
        .map(|s| {
            (
                s.id,
                s.heading.clone().unwrap_or_else(|| s.group_name.clone()),
            )
        })
        .collect();
    let mut rows: Vec<ComponentRow> = curated
        .iter()
        .map(|c| {
            let kind = kind_by_id.get(&c.target_id).copied().unwrap_or("");
            ComponentRow {
                section: c.section_id.and_then(|s| heading_of.get(&s).cloned()),
                target_id: c.target_id.to_string(),
                monitor_name: c.monitor_name.clone(),
                kind: kind_label(kind),
                name_hint: name_hint(kind),
                on_page: true,
                public_name: c.public_name.clone().unwrap_or_default(),
                public_group: c.public_group.clone().unwrap_or_default(),
                // A revoked share leaves the flag set; show what the page renders.
                detail_link: c.detail_link_enabled && c.share_live,
            }
        })
        .collect();
    let mut others: Vec<_> = all_targets
        .into_iter()
        .filter(|t| !on_page.contains(&t.id))
        .collect();
    others.sort_by_key(|t| t.name.to_lowercase());
    rows.extend(others.into_iter().map(|t| {
        let kind = t.check.kind();
        ComponentRow {
            kind: kind_label(kind),
            name_hint: name_hint(kind),
            target_id: t.id.to_string(),
            monitor_name: t.name,
            on_page: false,
            public_name: String::new(),
            public_group: String::new(),
            detail_link: false,
            section: None,
        }
    }));
    let sections: Vec<SectionRow> = sections
        .into_iter()
        .map(|s| SectionRow {
            id: s.id.to_string(),
            heading: s.heading.unwrap_or_default(),
            members: s.members.len(),
            left_off: s.left_off,
            group_name: s.group_name,
        })
        .collect();

    let cfg = &state.cfg.public_status;
    let b = &page.branding;
    let base = public_base(&state.cfg, &page.slug);
    let status_url = base
        .as_deref()
        .map(|o| public_status_url(&state.cfg, o))
        .unwrap_or_default();
    let (badge_url, badge_link) = badge_embed(&state.cfg, &page.slug);
    let logo_url = b
        .logo_hash
        .as_deref()
        .and_then(|hash| public_logo_url(base.as_deref(), hash))
        .unwrap_or_default();
    let (logo_w, logo_h) = if b.logo_hash.is_some() {
        match state
            .page_asset_store
            .get_meta(org, page_id, crate::domain::AssetSlot::Logo)
            .await?
        {
            Some(m) => (
                m.metadata
                    .get("width")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                m.metadata
                    .get("height")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            ),
            None => (0, 0),
        }
    } else {
        (0, 0)
    };
    let styles: Vec<StyleOption> = PublicStyle::ALL
        .iter()
        .map(|v| StyleOption {
            value: v,
            selected: *v == b.public_style.as_str(),
        })
        .collect();
    let locales: Vec<LocaleOption> = Locale::ALL
        .into_iter()
        .map(|l| LocaleOption {
            value: l.as_str(),
            label: l.native_name(),
            selected: l == page.public_locale,
        })
        .collect();

    let subscribers = match state.db.as_ref() {
        Some(pool) => crate::storage::subscribers::list_for_page(pool, org.0, page_id.0)
            .await?
            .into_iter()
            .map(|s| {
                let contact = if s.channel == "email" {
                    crate::email::mask_email(&s.target)
                } else {
                    s.target
                };
                SubscriberView {
                    id: s.id.to_string(),
                    contact,
                    channel: s.channel,
                    confirmed: s.verified,
                    created_at: s.created_at,
                    via_password: s.via_password_at.is_some(),
                    with_current_password: access.made_with_password(s.via_password_at),
                }
            })
            .collect(),
        None => Vec::new(),
    };
    let subscribers_with_password = subscribers
        .iter()
        .filter(|s| s.with_current_password)
        .count();

    let page_name = match state.db.as_ref() {
        Some(pool) => crate::storage::load_page_branding(pool, page_id)
            .await?
            .map(|ob| ob.resolved_display_name().to_owned())
            .unwrap_or_default(),
        None => String::new(),
    };
    let badge_alt = badge_alt_label(&page_name);
    let public_display_name = b.public_display_name.clone().unwrap_or_default();

    let mut groups: Vec<String> = rows
        .iter()
        .map(|c| c.public_group.trim())
        .filter(|g| !g.is_empty())
        .map(str::to_owned)
        .collect();
    groups.sort_unstable();
    groups.dedup();

    Ok(PageEditorPage {
        active_tab: TAB_PAGES,
        id: page.id.to_string(),
        slug: page.slug.clone(),
        name: page.name,
        enabled: page.enabled,
        public_display_name,
        public_about: b.public_about.clone().unwrap_or_default(),
        brand_color_value: b
            .public_brand_color
            .clone()
            .unwrap_or_else(|| cfg.default_brand_color.clone()),
        show_powered_by: b.public_show_powered_by,
        powered_by_default: cfg.default_show_powered_by,
        white_label,
        hide_from_search: b.public_hide_from_search,
        website_url: b.public_website_url.clone().unwrap_or_default(),
        styles,
        locales,
        groups,
        host_suffix: public_host_suffix(&state.cfg),
        status_url,
        badge_url,
        badge_link,
        badge_alt,
        logo_url,
        logo_w,
        logo_h,
        max_logo_bytes: u64::from(cfg.max_logo_size_bytes),
        max_logo_dim_px: cfg.max_logo_dimension_px,
        components: rows,
        sections,
        group_choices,
        subscribers,
        visibility,
        lock,
        access_json,
        password_json,
        subscribers_with_password,
        rule_count: access.rules.len(),
        viewers_active: counts.active,
        viewers_blocked: counts.blocked,
    }
    .into_response())
}

/// The badge image and the status page it links to. Both are absolute,
/// subdomain in SaaS mode, else the configured public base URL: the embed
/// lives on someone else's site, where a relative link points at that site.
fn badge_embed(cfg: &crate::config::AppConfig, slug: &str) -> (Option<String>, Option<String>) {
    let base = public_base(cfg, slug);
    let origin = match base.as_deref() {
        Some(own) if !own.is_empty() => own,
        _ => cfg.auth.public_base_url.trim_end_matches('/'),
    };
    if origin.is_empty() {
        return (None, None);
    }
    let link = base.is_some().then(|| {
        format!(
            "{}?utm_source=status-badge&utm_medium=badge&utm_campaign=embed",
            public_status_url(cfg, origin)
        )
    });
    (Some(format!("{origin}/api/public/v1/badge.svg")), link)
}

/// Overall-badge alt text. Strips `"` so the value can't break out of the
/// copied `alt="…"` attribute.
fn badge_alt_label(page_name: &str) -> String {
    crate::public_status::status_title(&page_name.replace('"', ""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    #[test]
    fn every_check_kind_has_label_and_hint() {
        for kind in crate::domain::CheckSpec::ALL_KINDS {
            assert_ne!(kind_label(kind), "Check", "fallback label for {kind}");
            assert_ne!(
                name_hint(kind),
                "Public display name",
                "fallback hint for {kind}"
            );
        }
    }

    fn editor(badge_url: Option<String>) -> PageEditorPage {
        PageEditorPage {
            active_tab: TAB_PAGES,
            id: "11111111-1111-1111-1111-111111111111".into(),
            slug: "acme".into(),
            name: "acme".into(),
            enabled: true,
            public_display_name: "Acme".into(),
            public_about: String::new(),
            brand_color_value: "#000000".into(),
            show_powered_by: None,
            powered_by_default: true,
            white_label: false,
            hide_from_search: false,
            website_url: String::new(),
            styles: Vec::new(),
            locales: Vec::new(),
            groups: Vec::new(),
            host_suffix: Some(".uptimepage.dev".into()),
            status_url: "https://acme.uptimepage.dev".into(),
            badge_url,
            badge_link: Some(
                "https://acme.uptimepage.dev?utm_source=status-badge&utm_medium=badge&utm_campaign=embed"
                    .into(),
            ),
            badge_alt: "Acme Status".into(),
            logo_url: String::new(),
            logo_w: 0,
            logo_h: 0,
            max_logo_bytes: 1024,
            max_logo_dim_px: 512,
            components: vec![ComponentRow {
                target_id: "22222222-2222-2222-2222-222222222222".into(),
                monitor_name: "api".into(),
                kind: "HTTP",
                name_hint: "e.g. Website",
                on_page: true,
                public_name: "API".into(),
                public_group: String::new(),
                detail_link: false,
                section: None,
            }],
            sections: Vec::new(),
            group_choices: Vec::new(),
            subscribers: Vec::new(),
            visibility: "public",
            lock: None,
            access_json: r#"{"protected":false,"session_days":null,"email":{"enabled":true,"passkeys_enabled":true,"recheck_days":null},"password":{"enabled":false},"rules":[]}"#.into(),
            password_json: "{}".into(),
            subscribers_with_password: 0,
            rule_count: 0,
            viewers_active: 0,
            viewers_blocked: 0,
        }
    }

    #[test]
    fn the_badge_toggle_is_live_on_white_label_and_dimmed_otherwise() {
        let mut locked = editor(None);
        locked.white_label = false;
        let html = locked.render().unwrap();
        assert!(html.contains(r#"id="f-powered-by""#));
        assert!(
            html.contains("disabled"),
            "a plan without white-label cannot remove the badge"
        );
        assert!(html.contains("comes with pro"));
        assert!(html.contains("const storedPoweredBy = null;"));
        assert!(html.contains("poweredByTouched ? poweredBy.checked : storedPoweredBy"));

        let mut sold = editor(None);
        sold.white_label = true;
        sold.show_powered_by = Some(false);
        let html = sold.render().unwrap();
        let tag = &html[html.find(r#"id="f-powered-by""#).expect("control renders")..];
        assert!(!tag[..tag.find('>').unwrap()].contains("disabled"));
        assert!(!html.contains("comes with pro"));
        assert!(html.contains("const storedPoweredBy = false;"));
    }

    #[test]
    fn badge_panel_renders_overall_and_per_component_snippets() {
        let html = editor(Some(
            "https://acme.uptimepage.dev/api/public/v1/badge.svg".into(),
        ))
        .render()
        .unwrap();
        assert!(html.contains("https://acme.uptimepage.dev/api/public/v1/badge.svg"));
        assert!(html.contains("?component=22222222-2222-2222-2222-222222222222"));
        assert!(html.contains("data-copy=\"#badge-md-overall\""));
        // Embed is a real backlink: wrapped anchor + linked markdown + UTM.
        assert!(html.contains("data-copy=\"#badge-html-overall\""));
        assert!(html.contains("rel=\"noopener\""));
        assert!(html.contains("utm_source=status-badge"));
        assert!(html.contains("[!["));
        assert!(html.contains("alt=\"Acme Status\""));
        assert!(
            html.contains(
                "&lt;a href=\"https://acme.uptimepage.dev?utm_source=status-badge&#38;utm_medium=badge\
                 &#38;utm_campaign=embed\" target=\"_blank\" rel=\"noopener\"&gt;\
                 &lt;img src=\"https://acme.uptimepage.dev/api/public/v1/badge.svg\""
            ),
            "{html}"
        );
    }

    /// An embed lives on someone else's site, so a path-based deploy's badge
    /// links its page by full address rather than `/status`.
    #[test]
    fn the_badge_links_its_page_by_full_address() {
        let mut cfg = crate::config::AppConfig::load().expect("config");
        cfg.tenancy.subdomain_public_routes = false;
        cfg.tenancy.path_based_public_routes = true;
        // Path mode serves the page on the public base URL, base domain or not.
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.public_base_url = "https://status.example.com/".into();
        let (src, href) = badge_embed(&cfg, "acme");
        assert_eq!(
            src.as_deref(),
            Some("https://status.example.com/api/public/v1/badge.svg")
        );
        assert_eq!(
            href.as_deref(),
            Some(
                "https://status.example.com/status\
                 ?utm_source=status-badge&utm_medium=badge&utm_campaign=embed"
            )
        );

        cfg.tenancy.subdomain_public_routes = true;
        cfg.tenancy.path_based_public_routes = false;
        cfg.public_status.base_domain = "example.com".into();
        let (src, href) = badge_embed(&cfg, "acme");
        assert_eq!(
            src.as_deref(),
            Some("https://acme.example.com/api/public/v1/badge.svg")
        );
        assert_eq!(
            href.as_deref(),
            Some(
                "https://acme.example.com?utm_source=status-badge&utm_medium=badge&utm_campaign=embed"
            )
        );
    }

    #[test]
    fn badge_alt_strips_quotes() {
        assert_eq!(badge_alt_label("Acme Public"), "Acme Public Status");
        assert_eq!(badge_alt_label("Acme \"Pro\""), "Acme Pro Status");
        assert_eq!(badge_alt_label("Acme status"), "Acme status");
    }

    /// Slug and branding ride one PATCH, so the editor must offer one save
    /// rather than the two buttons that used to hit the same endpoint.
    #[test]
    fn the_editor_saves_everything_through_one_form() {
        let html = editor(None).render().unwrap();
        assert!(html.contains(r#"id="page-form""#), "{html}");
        assert!(!html.contains(r#"id="slug-form""#), "{html}");
        assert!(!html.contains(r#"id="branding-form""#), "{html}");
        assert_eq!(html.matches("type=\"submit\"").count(), 1, "{html}");
    }

    /// A second listener still parses, but the outer one only registers the
    /// inner and never prevents the default — the browser submits natively,
    /// the page reloads, and every unsaved field is discarded.
    #[test]
    fn the_editor_registers_exactly_one_submit_handler() {
        let html = editor(None).render().unwrap();
        assert_eq!(
            html.matches("addEventListener('submit'").count(),
            1,
            "{html}"
        );
    }

    /// The warning ships in the markup and is revealed once the field is
    /// dirty, so a rename cannot ride along on a save unannounced.
    #[test]
    fn a_rename_carries_its_warning() {
        let html = editor(None).render().unwrap();
        assert!(html.contains(r#"id="slug-warning""#), "{html}");
        assert!(html.contains("hard cutover"), "{html}");
        assert!(html.contains(r#"data-original="acme""#), "{html}");
    }

    /// Who can open the page is a rail choice, not a checkbox buried in the form.
    #[test]
    fn the_rail_offers_public_private_and_off() {
        let html = editor(None).render().unwrap();
        assert!(
            html.contains(r#"name="visibility" value="public" checked"#),
            "{html}"
        );
        for v in ["private", "off"] {
            assert!(
                html.contains(&format!(r#"name="visibility" value="{v}""#)),
                "{v}"
            );
        }
        assert!(!html.contains(r#"id="f-enabled""#), "{html}");
    }

    fn styled_editor() -> PageEditorPage {
        let mut p = editor(None);
        p.styles = vec![
            StyleOption {
                value: "default",
                selected: false,
            },
            StyleOption {
                value: "nord",
                selected: true,
            },
        ];
        p.groups = vec!["Core".into(), "Edge".into()];
        p
    }

    /// A theme is a visual thing; picking one from a list of bare words means
    /// guessing what `dim` looks like versus `night`.
    #[test]
    fn themes_are_swatch_cards_wearing_their_own_palette() {
        let html = styled_editor().render().unwrap();
        assert!(html.contains(r#"class="theme-card theme-nord""#), "{html}");
        assert!(html.contains("theme-card__swatch--ink"), "{html}");
        assert!(!html.contains(r#"id="f-style""#), "{html}");
    }

    /// Two spellings of one group silently become two sections on the public
    /// page, so the groups already in use are offered back.
    #[test]
    fn existing_groups_are_offered_as_suggestions() {
        let html = styled_editor().render().unwrap();
        assert!(
            html.contains(r#"<datalist id="component-groups">"#),
            "{html}"
        );
        assert!(html.contains(r#"<option value="Core">"#), "{html}");
        assert!(html.contains(r#"list="component-groups""#), "{html}");
    }

    #[test]
    fn component_row_shows_check_kind() {
        let html = editor(None).render().unwrap();
        assert!(html.contains("HTTP"));
    }

    #[test]
    fn badge_panel_absent_without_badge_url() {
        let html = editor(None).render().unwrap();
        assert!(!html.contains("/api/public/v1/badge.svg"));
    }

    fn page_row(name: &str) -> PageRow {
        PageRow {
            id: "33333333-3333-3333-3333-333333333333".into(),
            name: name.into(),
            slug: name.into(),
            enabled: true,
            url: format!("https://{name}.uptimepage.dev"),
            subscriber_count: 1,
            plan_held: false,
            private: false,
            viewers: 0,
        }
    }

    #[test]
    fn a_private_page_card_names_viewers_and_what_a_hold_returns_it_to() {
        let mut private = page_row("weber");
        private.private = true;
        private.viewers = 3;
        let mut held = page_row("lindner");
        held.private = true;
        held.plan_held = true;
        let html = PagesPartial {
            rows: vec![private, held, page_row("acme")],
            at_limit: false,
            max_pages: None,
        }
        .render()
        .unwrap();
        assert!(html.contains("status-badge--private"), "{html}");
        assert!(html.contains("3 viewers"), "{html}");
        assert!(
            html.contains("back to private when the plan covers it"),
            "{html}"
        );
        assert!(html.contains(">public</span>"), "{html}");
    }

    #[test]
    fn the_editor_has_an_access_tab_and_locks_private_below_team() {
        let open = editor(None).render().unwrap();
        assert!(open.contains(r#"role="tablist""#));
        assert!(open.contains(r#"data-panel="access""#));
        assert!(open.contains("page_access_editor"));

        let mut locked = editor(None);
        locked.lock = Some(crate::web::views::team_lock::TeamLock {
            payer: true,
            teaser: true,
        });
        let html = locked.render().unwrap();
        assert!(
            !html.contains(r#"value="private""#),
            "no private radio below Team"
        );
        assert!(html.contains("on the Team plan"));
    }

    #[test]
    fn add_card_invites_a_new_page_below_the_cap() {
        let html = PagesPartial {
            rows: vec![page_row("acme")],
            at_limit: false,
            max_pages: Some(3),
        }
        .render()
        .unwrap();
        assert!(html.contains("smCreateDraftPage"), "{html}");
        assert!(html.contains("1 of 3 used"), "{html}");
        // A delete re-renders this partial, which is what re-reads the cap;
        // a full page reload would work but throws the scroll position away.
        assert!(html.contains("pages:refresh"), "{html}");
        assert!(!html.contains("location.reload"), "{html}");
    }

    /// At the cap the add card stays visible so the ceiling is legible, but
    /// offering a click the API would reject reads as the product misfiring.
    #[test]
    fn add_card_states_the_cap_instead_of_offering_a_page() {
        let html = PagesPartial {
            rows: vec![page_row("acme"), page_row("beta")],
            at_limit: true,
            max_pages: Some(2),
        }
        .render()
        .unwrap();
        assert!(!html.contains("smCreateDraftPage"), "{html}");
        assert!(html.contains(r#"card-badge--warn">2 / 2"#), "{html}");
    }

    /// The empty state has its own hero CTA; a lone add card would be a
    /// second, weaker front door.
    #[test]
    fn no_pages_renders_the_hero_and_no_grid() {
        let html = PagesPartial {
            rows: Vec::new(),
            at_limit: false,
            max_pages: Some(3),
        }
        .render()
        .unwrap();
        assert!(html.contains("create your first page"), "{html}");
        assert!(!html.contains("page-cards"), "{html}");
    }

    /// A plan can carry no pages at all, and the hero must not offer a button
    /// the API would reject — the cap governs the empty state too.
    #[test]
    fn a_cap_of_zero_leaves_the_hero_without_a_call_to_action() {
        let html = PagesPartial {
            rows: Vec::new(),
            at_limit: true,
            max_pages: Some(0),
        }
        .render()
        .unwrap();
        assert!(!html.contains("smCreateDraftPage"), "{html}");
        assert!(html.contains("move up a plan"), "{html}");
    }

    /// Self-host reports `i32::MAX`. Printing it would read as a real number.
    #[test]
    fn an_uncapped_plan_names_no_ceiling() {
        let html = PagesPartial {
            rows: vec![page_row("acme")],
            at_limit: false,
            max_pages: None,
        }
        .render()
        .unwrap();
        assert!(html.contains("smCreateDraftPage"), "{html}");
        assert!(!html.contains("used"), "{html}");
        assert!(!html.contains(&i32::MAX.to_string()), "{html}");
    }

    /// The help block sits inside the swapped region, so deleting the last
    /// page cannot leave it stranded above an empty state.
    #[test]
    fn the_help_block_travels_with_the_grid() {
        let listed = PagesPartial {
            rows: vec![page_row("acme")],
            at_limit: false,
            max_pages: Some(3),
        }
        .render()
        .unwrap();
        let empty = PagesPartial {
            rows: Vec::new(),
            at_limit: false,
            max_pages: Some(3),
        }
        .render()
        .unwrap();
        assert!(listed.contains("status-pages --help"), "{listed}");
        assert!(!empty.contains("status-pages --help"), "{empty}");
    }
}
