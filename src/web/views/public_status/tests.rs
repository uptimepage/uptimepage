use super::branding::*;
use super::og::*;
use super::urls::*;
use super::view::*;
use super::*;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::config::PublicStatusConfig;
use crate::domain::OverallStatus;
use crate::domain::{
    DayState, IncidentSeverity, IncidentStatusPhase, OverallState, PublicComponent,
    PublicComponentGroup, PublicComponentStatus, PublicIncident, PublicIncidentUpdate,
    PublicMaintenance, PublicOrgBranding, PublicStatusPage,
};
use crate::public_status::HistoryIncidentMarker;
use crate::storage::orgs::OrgBranding;

#[test]
fn powered_by_forced_for_saas_non_white_label() {
    // SaaS free: badge shown even if the page tried to hide it.
    assert!(enforce_powered_by(false, true, false));
    assert!(enforce_powered_by(true, true, false));
    // SaaS white-label (Pro): stored preference honoured.
    assert!(!enforce_powered_by(false, true, true));
    // Self-host: unrestricted, stored preference honoured.
    assert!(!enforce_powered_by(false, false, false));
}

fn sample_page() -> PublicStatusPage {
    PublicStatusPage {
        overall: OverallStatus {
            state: OverallState::Operational,
            label: "All Systems Operational".into(),
        },
        generated_at: Utc::now(),
        site_name: "Acme".into(),
        groups: vec![PublicComponentGroup {
            name: Some("API".into()),
            components: vec![PublicComponent {
                id: Uuid::nil(),
                name: "Gateway".into(),
                description: Some("Customer-facing edge".into()),
                current_status: PublicComponentStatus::Operational,
                history: vec![DayState::Operational; HISTORY_LEN],
                detail_url: None,
            }],
        }],
        active_incidents: vec![],
        recent_incidents: vec![],
        recent_incidents_has_more: false,
        active_maintenance: vec![],
        upcoming_maintenance: vec![],
    }
}

fn sample_branding() -> BrandingView {
    BrandingView::from_org(
        &OrgBranding {
            name: "Acme".into(),
            slug: "acme".into(),
            branding: PublicOrgBranding::default(),
        },
        &PublicStatusConfig::default(),
        "/",
    )
}

#[test]
fn full_page_renders_chrome_and_components() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("Acme Status"));
    assert!(html.contains("All Systems Operational"));
    assert!(html.contains("Gateway"));
    // Relative so the poll stays on whichever URL served the page.
    assert!(html.contains(r#"hx-get="?fragment=1""#));
    assert!(html.contains(r#"hx-trigger="every 30s, sm:poll-resume from:body""#));
    // An unpublished page 404s the fragment; reload rather than ask forever.
    assert!(html.contains("data-reload-on-404"));
    assert!(html.contains("data-tz"));
    assert!(html.contains(&crate::web::assets::url("js/htmx.min.js")));
    // Without it nothing honours [data-poll-pause] and the region polls on.
    assert!(html.contains(&crate::web::assets::url("js/ui/polling.js")));
    assert!(html.contains(&crate::web::assets::url("js/ui/localtime.js")));
    assert!(html.contains(&crate::web::assets::url("js/public/day_popover.js")));
    assert!(html.contains("/api/public/v1/incidents.rss"));
}

#[test]
fn day_strip_renders_trigger_buttons_and_blob() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusRegion { view }.render().unwrap();
    assert!(html.contains("data-day-trigger"));
    assert!(html.contains(r#"data-comp="#));
    assert!(html.contains(r#"data-day="#));
    // Shared popover + content template + JSON blob — each appears once.
    assert_eq!(html.matches(r#"id="day-popover""#).count(), 1);
    assert_eq!(html.matches(r#"id="day-popover-related-tpl""#).count(), 1);
    assert_eq!(html.matches(r#"id="day-strip-data""#).count(), 1);
    // Roving tabindex: one `tabindex="0"` per component strip; the rest
    // are `tabindex="-1"`. Sample has 1 component × 90 days = 1 stop.
    assert_eq!(html.matches(r#"tabindex="0""#).count(), 1);
    assert_eq!(html.matches(r#"tabindex="-1""#).count(), 89);
}

#[test]
fn day_strip_blob_links_overlapping_incident() {
    let mut p = sample_page();
    // Replace `today` with a bad day, then add a marker for a 40m
    // incident on the matching component.
    let comp_id = p.groups[0].components[0].id;
    let last = p.groups[0].components[0].history.len() - 1;
    p.groups[0].components[0].history[last] = DayState::MajorOutage;
    let marker = HistoryIncidentMarker {
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "Edge nodes returning 502".into(),
        started_at: p.generated_at - ChronoDuration::minutes(45),
        ended_at: Some(p.generated_at - ChronoDuration::minutes(5)),
    };
    let view = build_view(&p, &[marker], &Default::default());
    // The JSON blob is the data source — assert against it, not the
    // rendered <li> markup (the JS builds those at runtime).
    assert!(view.day_strip_json.contains("Edge nodes returning 502"));
    assert!(view.day_strip_json.contains("day-pop-status--maj"));
    assert!(view.day_strip_json.contains("40m"));
    assert!(view.day_strip_json.contains("\"show_badge\":true"));
}

#[test]
fn day_strip_blob_html_safe() {
    // An attacker-controlled incident title may not introduce ANY raw
    // `<`, `>`, or `&` into the inline JSON — those would let a title
    // close the <script>, open a comment (`<!--`), or break CDATA.
    let p = sample_page();
    let comp_id = p.groups[0].components[0].id;
    let marker = HistoryIncidentMarker {
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "evil </script><!--<img src=x>& bad".into(),
        started_at: p.generated_at - ChronoDuration::minutes(30),
        ended_at: Some(p.generated_at - ChronoDuration::minutes(5)),
    };
    let view = build_view(&p, &[marker], &Default::default());
    assert!(!view.day_strip_json.contains('<'));
    assert!(!view.day_strip_json.contains('>'));
    // `&` appears only as `&` — never raw.
    assert!(!view.day_strip_json.contains('&'));
    assert!(view.day_strip_json.contains("\\u003c"));
    // Round-trip safety: the encoded blob still parses back to the
    // original string after JSON.parse.
    let parsed: serde_json::Value = serde_json::from_str(&view.day_strip_json).expect("valid json");
    let title = parsed[comp_id.to_string()]["days"]
        .as_array()
        .and_then(|days| {
            days.iter()
                .rev()
                .find(|d| !d["related"].as_array().unwrap().is_empty())
        })
        .map(|d| d["related"][0]["title"].as_str().unwrap().to_string())
        .expect("found title");
    assert_eq!(title, "evil </script><!--<img src=x>& bad");
}

#[test]
fn day_overlap_clamps_to_day_window() {
    let comp_id = Uuid::new_v4();
    let now = Utc::now();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|nd| DateTime::<Utc>::from_naive_utc_and_offset(nd, Utc))
        .unwrap();
    let day_end = day_start + ChronoDuration::days(1);
    let inc = HistoryIncidentMarker {
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "spans midnight".into(),
        started_at: day_start - ChronoDuration::hours(3),
        ended_at: Some(day_start + ChronoDuration::hours(2)),
    };
    let pool: Vec<&HistoryIncidentMarker> = vec![&inc];
    let (dur, links) = day_overlap(&pool, day_start, day_end, now);
    assert_eq!(links.len(), 1);
    assert_eq!(dur, ChronoDuration::hours(2));
    let empty: Vec<&HistoryIncidentMarker> = Vec::new();
    let (dur2, links2) = day_overlap(&empty, day_start, day_end, now);
    assert!(links2.is_empty());
    assert_eq!(dur2, ChronoDuration::zero());
}

#[test]
fn fragment_renders_region_without_doctype() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusRegion { view }.render().unwrap();
    assert!(!html.contains("<!doctype html>"));
    assert!(!html.contains("<nav"));
    assert!(html.contains(r#"id="status-region""#));
    assert!(html.contains(r#"hx-trigger="every 30s, sm:poll-resume from:body""#));
    assert!(html.contains("Gateway"));
    // Re-armed on every swap, so both markers survive the zone replacing itself.
    assert!(html.contains("data-poll-pause"));
    assert!(html.contains("data-reload-on-404"));
    // An outerHTML swap inserts every top-level node beside the target.
    assert!(html.starts_with("<div id=\"status-region\""));
    assert!(html.ends_with("</div>"));
}

#[test]
fn empty_page_renders_with_zero_components() {
    let mut p = sample_page();
    p.groups.clear();
    let view = build_view(&p, &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("No public components"));
}

#[test]
fn active_incident_banner_renders_when_present() {
    let mut p = sample_page();
    p.overall = OverallStatus {
        state: OverallState::MinorDisruption,
        label: "Minor Service Disruption".into(),
    };
    p.active_incidents.push(PublicIncident {
        id: Uuid::nil(),
        component_id: Uuid::nil(),
        component_name: "Gateway".into(),
        title: "Latency spike".into(),
        started_at: Utc::now() - ChronoDuration::minutes(14),
        ended_at: None,
        severity: IncidentSeverity::Major,
        status_phase: IncidentStatusPhase::Identified,
        updates: vec![PublicIncidentUpdate {
            posted_at: Utc::now() - ChronoDuration::minutes(2),
            phase: IncidentStatusPhase::Identified,
            message: "Rolling back the deploy.".into(),
        }],
        postmortem: None,
    });
    let view = build_view(&p, &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Active incident"));
    assert!(html.contains("Latency spike"));
    assert!(html.contains("Identified"));
    assert!(html.contains("Rolling back the deploy."));
}

#[test]
fn maintenance_card_renders_when_present() {
    let mut p = sample_page();
    p.active_maintenance.push(PublicMaintenance {
        id: Uuid::nil(),
        title: "DB upgrade".into(),
        description: Some("Brief".into()),
        starts_at: Utc::now() - ChronoDuration::minutes(5),
        ends_at: Utc::now() + ChronoDuration::hours(1),
        affected_component_names: vec!["Gateway".into()],
    });
    let view = build_view(&p, &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Scheduled maintenance"));
    assert!(html.contains("DB upgrade"));
    assert!(html.contains("Gateway"));
}

#[test]
fn day_classes_cover_all_states() {
    for s in [
        DayState::Operational,
        DayState::Degraded,
        DayState::PartialOutage,
        DayState::MajorOutage,
        DayState::Maintenance,
        DayState::NoData,
    ] {
        let (class, label, tint) = day_classes(s);
        assert!(!class.is_empty());
        assert!(!label.is_empty());
        assert!(tint.starts_with("day-pop-status--"));
    }
}

#[test]
fn history_stats_computes_uptime() {
    let mut h = vec![DayState::Operational; 90];
    h[10] = DayState::MajorOutage;
    let (pct, summary) = history_stats(&h);
    assert!(pct.starts_with("98"));
    assert!(summary.contains("1 outage"));
}

#[test]
fn incident_detail_renders() {
    let inc = PublicIncident {
        id: Uuid::nil(),
        component_id: Uuid::nil(),
        component_name: "Gateway".into(),
        title: "Latency spike".into(),
        started_at: Utc::now() - ChronoDuration::minutes(30),
        ended_at: Some(Utc::now()),
        severity: IncidentSeverity::Major,
        status_phase: IncidentStatusPhase::Resolved,
        updates: vec![
            PublicIncidentUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(25),
                phase: IncidentStatusPhase::Investigating,
                message: "Looking into it.".into(),
            },
            PublicIncidentUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(5),
                phase: IncidentStatusPhase::Resolved,
                message: "Rolled back the deploy.".into(),
            },
        ],
        postmortem: None,
    };
    let detail = IncidentDetailView::from_incident(&inc, Utc::now());
    let html = IncidentDetailPage {
        branding: sample_branding(),
        incident: detail,
        generated_at: Utc::now(),
        rss_url: RSS_URL,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("Latency spike"));
    assert!(html.contains("Investigating"));
    assert!(html.contains("Resolved"));
    assert!(html.contains("Rolled back the deploy."));
}

#[test]
fn incident_detail_renders_published_postmortem() {
    let mut inc = fake_incident(Utc::now() - ChronoDuration::hours(2), 9, "DB outage");
    inc.postmortem = Some(crate::domain::PublicPostmortem {
        summary: Some("Connection pool exhausted.".into()),
        root_cause: Some("Missing timeout.".into()),
        impact: None,
        action_items: vec![crate::domain::PublicActionItem {
            text: "Cap pool checkout".into(),
            done: true,
        }],
        published_at: Utc::now(),
    });
    let html = IncidentDetailPage {
        branding: sample_branding(),
        incident: IncidentDetailView::from_incident(&inc, Utc::now()),
        generated_at: Utc::now(),
        rss_url: RSS_URL,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Postmortem"));
    assert!(html.contains("Connection pool exhausted."));
    assert!(html.contains("Cap pool checkout"));

    // No postmortem → no section.
    let mut plain = fake_incident(Utc::now() - ChronoDuration::hours(2), 8, "Blip");
    plain.postmortem = None;
    let html2 = IncidentDetailPage {
        branding: sample_branding(),
        incident: IncidentDetailView::from_incident(&plain, Utc::now()),
        generated_at: Utc::now(),
        rss_url: RSS_URL,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(!html2.contains("Postmortem"));
}

fn branding_with(b: PublicOrgBranding) -> BrandingView {
    BrandingView::from_org(
        &OrgBranding {
            name: "Acme".into(),
            slug: "acme".into(),
            branding: b,
        },
        &PublicStatusConfig::default(),
        "/",
    )
}

#[test]
fn safe_brand_color_accepts_strict_hex_only() {
    assert_eq!(safe_brand_color(Some("#a1B2c3"), "#000000"), "#a1B2c3");
    for bad in [
        "blue",
        "#abc",
        "#3b82f",
        "#3b82f60",
        "#zzzzzz",
        "  #3b82f6",
        "red; } body { display: none }",
    ] {
        assert_eq!(
            safe_brand_color(Some(bad), "#3b82f6"),
            "#3b82f6",
            "should reject {bad:?}"
        );
    }
    assert_eq!(safe_brand_color(None, "#3b82f6"), "#3b82f6");
}

#[test]
fn brand_text_picks_white_on_very_dark_brands() {
    for dark in ["#000000", "#1e3a8a", "#064e3b", "#4c1d95", "#7c2d12"] {
        assert_eq!(
            safe_brand_text_for(dark),
            "#ffffff",
            "{dark} should pair with white"
        );
    }
}

#[test]
fn brand_text_picks_dark_on_anything_brighter() {
    // Pastels, mid-tones (Twitter blue, Slack yellow, Tailwind 500s) all
    // give better contrast against near-black than white per WCAG.
    for bright in [
        "#ffffff", "#ffee99", "#facc15", "#bef264", "#fde68a", "#1d9bf0", "#3b82f6", "#06b6d4",
        "#22c55e", "#ecb22e",
    ] {
        assert_eq!(
            safe_brand_text_for(bright),
            "#0f172a",
            "{bright} should pair with near-black"
        );
    }
}

#[test]
fn brand_text_falls_back_to_white_on_malformed() {
    for bad in ["", "blue", "#fff", "#zzzzzz"] {
        assert_eq!(safe_brand_text_for(bad), "#ffffff");
    }
}

#[test]
fn render_about_strips_disallowed_and_keeps_allowlist() {
    let html = render_about("**bold** _em_ <script>alert(1)</script>\n\n- a\n- b");
    assert!(html.contains("<strong>bold</strong>"));
    assert!(html.contains("<em>em</em>"));
    assert!(html.contains("<li>a</li>"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("alert(1)"));
}

#[test]
fn render_about_adds_rel_to_links() {
    let html = render_about("[x](https://example.com)");
    assert!(html.contains("href=\"https://example.com\""));
    assert!(html.contains("rel=\"noopener nofollow\""));
}

#[test]
fn branding_renders_logo_about_and_color() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let branding = branding_with(PublicOrgBranding {
        public_display_name: Some("Acme Public".into()),
        public_about: Some("**hi** there".into()),
        public_brand_color: Some("#ff0000".into()),
        logo_hash: Some("deadbeefcafef00d".into()),
        public_show_powered_by: Some(false),
        ..PublicOrgBranding::default()
    });
    let html = StatusFullPage {
        view,
        branding,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Acme Public Status"));
    assert!(html.contains("--brand-color: #ff0000;"));
    assert!(html.contains(r#"src="/status/branding/logo?v=deadbeefcafef00d""#));
    assert!(html.contains("<strong>hi</strong> there"));
    assert!(!html.contains("Powered by"));
}

#[test]
fn og_tags_render_with_image_when_marketing_origin_set() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta {
            title: "Acme Status".into(),
            description: "Live operational status for Acme.".into(),
            og_type: "website",
            url: "https://acme.uptimepage.dev/status".into(),
            image: "https://uptimepage.dev/static/marketing/og-status.png".into(),
            site_name: "Acme".into(),
        },
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<meta property="og:title" content="Acme Status">"#));
    assert!(html.contains(r#"<meta property="og:site_name" content="Acme">"#));
    assert!(html.contains(r#"<link rel="canonical" href="https://acme.uptimepage.dev/status">"#));
    assert!(
        html.contains(r#"<meta property="og:url" content="https://acme.uptimepage.dev/status">"#)
    );
    assert!(html.contains(
        r#"<meta property="og:image" content="https://uptimepage.dev/static/marketing/og-status.png">"#
    ));
    assert!(html.contains(r#"<meta name="twitter:card" content="summary_large_image">"#));
    assert!(html.contains(
        r#"<meta name="twitter:image" content="https://uptimepage.dev/static/marketing/og-status.png">"#
    ));
}

#[test]
fn incident_description_drops_the_subject_when_the_component_is_unnamed() {
    assert!(
        incident_description("Checkout 500s", "API", "Acme")
            .starts_with("Checkout 500s, affecting API:")
    );
    assert!(
        incident_description("Checkout 500s", "", "Acme").starts_with("Checkout 500s: current")
    );
}

#[test]
fn incident_descriptions_differ_per_incident() {
    assert_ne!(
        incident_description("Checkout 500s", "API", "Acme"),
        incident_description("Latency spike", "API", "Acme")
    );
}

#[test]
fn og_image_is_never_the_marketing_card() {
    let image = og_image("https://uptimepage.dev");
    assert_eq!(
        image,
        "https://uptimepage.dev/static/marketing/og-status.png"
    );
}

#[test]
fn og_image_is_empty_without_marketing_origin() {
    assert!(og_image("").is_empty());
}

#[test]
fn og_tags_fall_back_to_summary_when_image_empty() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta {
            title: "Acme Status".into(),
            description: "Live operational status for Acme.".into(),
            og_type: "website",
            url: String::new(),
            image: String::new(),
            site_name: String::new(),
        },
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<meta name="twitter:card" content="summary">"#));
    assert!(!html.contains("og:image"));
    assert!(!html.contains("og:site_name"));
    assert!(!html.contains("og:url"));
    assert!(!html.contains("rel=\"canonical\""));
    assert!(!html.contains("twitter:image"));
}

#[test]
fn powered_by_shown_by_default() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let html = StatusFullPage {
        view,
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Powered by"));
    assert!(
        html.contains(r#"<a href="https://uptimepage.dev" class="hover:text-body">uptimepage</a>"#)
    );
}

// PRE-MORTEM PM #6: a relaxed DB/app validator must not let a crafted
// brand colour break out of the `:root` rule. The template-side
// sanitiser is the independent layer that holds even then.
#[test]
fn malicious_brand_color_cannot_escape_style_rule() {
    let view = build_view(&sample_page(), &[], &Default::default());
    let branding = branding_with(PublicOrgBranding {
        public_brand_color: Some("red; } body { display: none } /*".into()),
        ..PublicOrgBranding::default()
    });
    let html = StatusFullPage {
        view,
        branding,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    // Exactly one `--brand-color:` declaration, and it is the default —
    // `var(--brand-color)` uses have no colon so they don't match.
    assert_eq!(html.matches("--brand-color:").count(), 1);
    assert!(html.contains("--brand-color: #3b82f6;"));
    assert!(!html.contains("display: none"));
    assert!(!html.contains("} body {"));
}

fn fake_incident(started_at: DateTime<Utc>, id_low: u8, title: &str) -> PublicIncident {
    let mut id_bytes = [0u8; 16];
    id_bytes[15] = id_low;
    PublicIncident {
        id: Uuid::from_bytes(id_bytes),
        component_id: Uuid::nil(),
        component_name: "API".into(),
        title: title.into(),
        started_at,
        ended_at: Some(started_at + ChronoDuration::minutes(15)),
        severity: IncidentSeverity::Minor,
        status_phase: IncidentStatusPhase::Resolved,
        updates: Vec::new(),
        postmortem: None,
    }
}

#[test]
fn bucket_by_month_groups_consecutive_incidents() {
    let now = Utc::now();
    let may_a = chrono::DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let may_b = chrono::DateTime::parse_from_rfc3339("2026-05-01T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let apr = chrono::DateTime::parse_from_rfc3339("2026-04-15T11:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let items = vec![
        fake_incident(may_a, 1, "May late"),
        fake_incident(may_b, 2, "May early"),
        fake_incident(apr, 3, "April"),
    ];
    let buckets = bucket_by_month(&items, now);
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].label, "May 2026");
    assert_eq!(buckets[0].incidents.len(), 2);
    assert_eq!(buckets[1].label, "April 2026");
    assert_eq!(buckets[1].incidents.len(), 1);
}

#[test]
fn archive_page_renders_buckets_and_next_link() {
    let now = Utc::now();
    let started = chrono::DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let items = vec![fake_incident(started, 1, "ECS rolled back")];
    let months = bucket_by_month(&items, now);
    let page = IncidentArchivePage {
        branding: sample_branding(),
        months,
        next_cursor: Some("opaque-cursor-token".into()),
        rss_url: RSS_URL,
        robots: "index,follow",
        og: OgMeta::default(),
    };
    let html = page.render().unwrap();
    assert!(html.contains("Incident history"));
    assert!(html.contains("May 2026"));
    assert!(html.contains("ECS rolled back"));
    assert!(
        html.contains("/status/incidents?cursor=opaque-cursor-token"),
        "next-page link must include cursor"
    );
}

#[test]
fn status_url_puts_the_page_where_the_deploy_serves_it() {
    assert_eq!(
        status_url_for(true, "https://acme.example.com"),
        "https://acme.example.com"
    );
    assert_eq!(
        status_url_for(false, "https://status.acme.test"),
        "https://status.acme.test/status"
    );
}

#[test]
fn only_the_cursorless_archive_page_is_indexable() {
    let branding = sample_branding();
    assert_eq!(archive_robots(None, &branding), "index,follow");
    assert_eq!(
        archive_robots(Some("opaque-cursor-token"), &branding),
        "noindex,follow"
    );
}

#[test]
fn only_a_plan_that_sells_white_label_follows_the_operators_link() {
    let page = |follow: bool| {
        let mut branding = branding_with(PublicOrgBranding {
            public_website_url: Some("https://acme.example".into()),
            ..Default::default()
        });
        branding.follow_website_link = follow;
        StatusFullPage {
            view: build_view(&sample_page(), &[], &Default::default()),
            branding,
            og: OgMeta::default(),
        }
        .render()
        .unwrap()
    };
    assert!(page(true).contains(r#"<a href="https://acme.example""#));
    assert!(!page(true).contains("nofollow"));
    assert!(page(false).contains(r#"<a href="https://acme.example" rel="nofollow""#));
}

#[test]
fn follow_link_needs_white_label_on_hosted_and_nothing_on_self_host() {
    // Hosted: the plan decides.
    assert!(!enforce_follow_link(true, false, false));
    assert!(enforce_follow_link(true, true, false));
    // Self-host: no open signup to police, so the operator's own link follows.
    assert!(enforce_follow_link(false, false, false));
    // A page kept out of search passes nothing, whatever the plan.
    assert!(!enforce_follow_link(true, true, true));
    assert!(!enforce_follow_link(false, false, true));
}

#[test]
fn the_header_brand_falls_back_to_the_status_page_root() {
    let html = StatusFullPage {
        view: build_view(&sample_page(), &[], &Default::default()),
        branding: sample_branding(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<a href="/""#));
    assert!(!html.contains("nofollow"), "an internal link takes no rel");
}

#[test]
fn hiding_a_page_from_search_takes_every_page_with_it() {
    let branding = branding_with(PublicOrgBranding {
        public_hide_from_search: true,
        ..Default::default()
    });
    assert_eq!(branding.robots(), "noindex,follow");
    assert_eq!(archive_robots(None, &branding), "noindex,follow");

    let html = StatusFullPage {
        view: build_view(&sample_page(), &[], &Default::default()),
        branding,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<meta name="robots" content="noindex,follow">"#));
}

#[test]
fn archive_page_renders_empty_state_without_next_link() {
    let page = IncidentArchivePage {
        branding: sample_branding(),
        months: Vec::new(),
        next_cursor: None,
        rss_url: RSS_URL,
        robots: "noindex,follow",
        og: OgMeta::default(),
    };
    let html = page.render().unwrap();
    assert!(html.contains("No incidents recorded."));
    assert!(!html.contains("Older incidents"));
    assert!(html.contains(r#"<meta name="robots" content="noindex,follow">"#));
}

#[test]
fn branding_defaults_when_all_fields_null() {
    // With every override unset, the display name falls back to the org
    // name and no logo image is emitted (the header shows text). The
    // default colour and powered-by footer are covered by their own
    // tests above; this one pins the resolved-name + no-logo path.
    let view = build_view(&sample_page(), &[], &Default::default());
    let branding = branding_with(PublicOrgBranding::default());
    let html = StatusFullPage {
        view,
        branding,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();

    assert!(html.contains("Acme Status"), "display name = org name");
    assert!(
        !html.contains("/status/branding/logo"),
        "no logo img when path unset"
    );
}
