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
use crate::domain::{OrgId, PublicStyle};
use crate::error::AppError;
use crate::web::auth::CurrentOrg;
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::public_status::{
    public_base, public_host_suffix, public_logo_url, public_status_url,
};
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
    Ok(pages
        .into_iter()
        .map(|p| {
            let url = public_base(&state.cfg, &p.slug)
                .map(|o| public_status_url(&state.cfg, &o))
                .unwrap_or_default();
            let subscriber_count = counts.get(&p.id.0).copied().unwrap_or(0);
            PageRow {
                id: p.id.to_string(),
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
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "http" => "HTTP",
        "tcp" => "TCP",
        "ping" => "Ping",
        "heartbeat" => "Heartbeat",
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

/// One subscriber row in the editor's roster. `contact` is masked for email.
pub struct SubscriberView {
    pub id: String,
    pub contact: String,
    pub channel: String,
    pub confirmed: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
    /// Confirmed + pending subscribers, newest first.
    pub subscribers: Vec<SubscriberView>,
}

pub async fn page_editor(
    State(state): State<AppState>,
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

    let white_label = state.quotas.limit_for_org(org).await?.white_label_enabled;

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
    let mut rows: Vec<ComponentRow> = curated
        .iter()
        .map(|c| {
            let kind = kind_by_id.get(&c.target_id).copied().unwrap_or("");
            ComponentRow {
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
        }
    }));

    let cfg = &state.cfg.public_status;
    let b = &page.branding;
    let base = public_base(&state.cfg, &page.slug);
    let status_url = base
        .as_deref()
        .map(|o| public_status_url(&state.cfg, o))
        .unwrap_or_default();
    // Absolute origin for the badge: subdomain in SaaS mode, else the
    // configured public base URL so path-based/self-host deploys still get a
    // README-ready link.
    let badge_origin = crate::web::host::page_origin(
        &state.cfg.public_status.base_domain,
        &state.cfg.auth.public_base_url,
        &page.slug,
        None,
        false,
    );
    let badge_url = (!badge_origin.is_empty()).then(|| {
        format!(
            "{}/api/public/v1/badge.svg",
            badge_origin.trim_end_matches('/')
        )
    });
    let badge_link = (!status_url.is_empty()).then(|| {
        format!("{status_url}?utm_source=status-badge&utm_medium=badge&utm_campaign=embed")
    });
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
                }
            })
            .collect(),
        None => Vec::new(),
    };

    let public_display_name = b.public_display_name.clone().unwrap_or_default();
    let badge_alt = badge_alt_label(&public_display_name, &page.name);

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
        subscribers,
    }
    .into_response())
}

/// Overall-badge alt text: prefer the public display name, else the page name.
/// Strips `"` so the value can't break out of the copied `alt="…"` attribute.
fn badge_alt_label(display_name: &str, name: &str) -> String {
    let label = if display_name.is_empty() {
        name
    } else {
        display_name
    };
    label.replace('"', "")
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
            groups: Vec::new(),
            host_suffix: Some(".uptimepage.dev".into()),
            status_url: "https://acme.uptimepage.dev".into(),
            badge_url,
            badge_link: Some(
                "https://acme.uptimepage.dev?utm_source=status-badge&utm_medium=badge&utm_campaign=embed"
                    .into(),
            ),
            badge_alt: "Acme".into(),
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
            }],
            subscribers: Vec::new(),
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
        assert!(html.contains("alt=\"Acme status\""));
    }

    #[test]
    fn badge_alt_prefers_display_name_and_strips_quotes() {
        assert_eq!(badge_alt_label("", "acme"), "acme");
        assert_eq!(badge_alt_label("Acme Public", "acme"), "Acme Public");
        assert_eq!(badge_alt_label("Acme \"Pro\"", "acme"), "Acme Pro");
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

    /// Publish state is a rail choice now, not a checkbox buried in the form.
    #[test]
    fn the_rail_offers_publish_and_draft() {
        let html = editor(None).render().unwrap();
        assert!(
            html.contains(r#"name="enabled" value="1" checked"#),
            "{html}"
        );
        assert!(html.contains(r#"name="enabled" value="0""#), "{html}");
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
        }
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
