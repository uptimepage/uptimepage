use super::incident_display::*;
use super::og::*;
use super::view::*;
use super::*;
use crate::public_status::urls::*;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::domain::OverallStatus;
use crate::domain::{
    DayState, Downtime, IncidentImpact, IncidentSeverity, IncidentStatusPhase,
    MaintenanceUpdatePhase, OverallState, PublicComponent, PublicComponentGroup,
    PublicComponentStatus, PublicIncident, PublicIncidentUpdate, PublicMaintenance,
    PublicMaintenanceUpdate, PublicOrgBranding, PublicStatusPage,
};
use crate::i18n::Tr;
use crate::public_status::history::HistoryEntry;
use crate::public_status::{HistoryMarker, MarkerKind};
use crate::web::views::page_branding::BrandingView;

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
                downtime: vec![Downtime::default(); HISTORY_LEN],
                checked: Vec::new(),
                uptime_pct: Some(99.9),
                detail_url: None,
            }],
        }],
        active_incidents: vec![],
        recent_incidents: vec![],
        recent_incidents_has_more: false,
        recent_maintenance: Vec::new(),
        recent_maintenance_has_more: false,
        active_maintenance: vec![],
        upcoming_maintenance: vec![],
    }
}

fn all_tab() -> HistoryRequest {
    HistoryRequest {
        tab: HistoryTab::All,
        home: "/",
    }
}

#[test]
fn full_page_renders_chrome_and_components() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
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
    assert!(html.contains(&crate::templates::assets::url("js/htmx.min.js")));
    // Without it nothing honours [data-poll-pause] and the region polls on.
    assert!(html.contains(&crate::templates::assets::url("js/ui/polling.js")));
    assert!(html.contains(&crate::templates::assets::url("js/ui/localtime.js")));
    assert!(html.contains(&crate::templates::assets::url("js/public/day_popover.js")));
    assert!(html.contains("/api/public/v1/incidents.rss"));
}

#[test]
fn day_strip_renders_trigger_buttons_and_blob() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains("data-day-trigger"));
    assert!(html.contains(r#"data-comp="#));
    assert!(html.contains(r#"data-day="#));
    // Shared popover + content template + JSON blob — each appears once.
    assert_eq!(html.matches(r#"id="day-popover""#).count(), 1);
    assert_eq!(html.matches(r#"id="day-popover-related-tpl""#).count(), 1);
    assert_eq!(html.matches(r#"id="day-popover-span-tpl""#).count(), 1);
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
    p.groups[0].components[0].downtime[last] = Downtime {
        major_secs: 40 * 60,
        partial_secs: 5 * 60,
        degraded_secs: 0,
    };
    let marker = HistoryMarker {
        kind: MarkerKind::Incident,
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "Edge nodes returning 502".into(),
        started_at: p.generated_at - ChronoDuration::minutes(45),
        ended_at: Some(p.generated_at - ChronoDuration::minutes(5)),
    };
    let view = build_view(&p, &[marker], &Default::default(), all_tab(), Tr::default());
    // The JSON blob is the data source — assert against it, not the
    // rendered <li> markup (the JS builds those at runtime).
    assert!(view.day_strip_json.contains("Edge nodes returning 502"));
    let blob: serde_json::Value = serde_json::from_str(&view.day_strip_json).unwrap();
    let today = &blob[comp_id.to_string()]["days"][last];
    assert_eq!(
        today["spans"],
        serde_json::json!([
            { "state": "Major outage", "state_class": "day-pop-status--maj", "duration": "40m" },
            { "state": "Partial outage", "state_class": "day-pop-status--part", "duration": "5m" },
        ]),
        "worst first, each state with its own time"
    );
    assert_eq!(
        blob[comp_id.to_string()]["days"][0]["spans"],
        serde_json::json!([])
    );
}

#[test]
fn a_day_cell_is_coloured_by_how_long_it_was_down() {
    let mut p = sample_page();
    let c = &mut p.groups[0].components[0];
    let n = c.history.len();
    // A four-minute total outage, then a day that lost an hour and a half.
    c.history[n - 2] = DayState::MajorOutage;
    c.downtime[n - 2].major_secs = 4 * 60;
    c.history[n - 1] = DayState::MajorOutage;
    c.downtime[n - 1].major_secs = 90 * 60;
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let cells = &view.groups[0].components[0].history;
    assert_eq!(cells[n - 2].class, "day-cell--deg");
    assert_eq!(cells[n - 1].class, "day-cell--maj");
    assert!(
        cells[n - 2].aria_label.ends_with("— Major outage 4m"),
        "{}",
        cells[n - 2].aria_label
    );
    assert!(cells[0].aria_label.ends_with("— Operational"));
}

#[test]
fn day_strip_blob_html_safe() {
    // An attacker-controlled incident title may not introduce ANY raw
    // `<`, `>`, or `&` into the inline JSON — those would let a title
    // close the <script>, open a comment (`<!--`), or break CDATA.
    let p = sample_page();
    let comp_id = p.groups[0].components[0].id;
    let marker = HistoryMarker {
        kind: MarkerKind::Incident,
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "evil </script><!--<img src=x>& bad".into(),
        started_at: p.generated_at - ChronoDuration::minutes(30),
        ended_at: Some(p.generated_at - ChronoDuration::minutes(5)),
    };
    let view = build_view(&p, &[marker], &Default::default(), all_tab(), Tr::default());
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
fn day_related_keeps_incidents_touching_the_day() {
    let comp_id = Uuid::new_v4();
    let now = Utc::now();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|nd| DateTime::<Utc>::from_naive_utc_and_offset(nd, Utc))
        .unwrap();
    let day_end = day_start + ChronoDuration::days(1);
    let inc = HistoryMarker {
        kind: MarkerKind::Incident,
        id: Uuid::new_v4(),
        component_id: comp_id,
        title: "spans midnight".into(),
        started_at: day_start - ChronoDuration::hours(3),
        ended_at: Some(day_start + ChronoDuration::hours(2)),
    };
    let pool: Vec<&HistoryMarker> = vec![&inc];
    assert_eq!(day_related(&pool, day_start, day_end, now).len(), 1);
    let before = day_start - ChronoDuration::days(1);
    assert!(day_related(&pool, before - ChronoDuration::days(1), before, now).is_empty());
    let empty: Vec<&HistoryMarker> = Vec::new();
    assert!(day_related(&empty, day_start, day_end, now).is_empty());
}

#[test]
fn fragment_renders_region_without_doctype() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(!html.contains("<!doctype html>"));
    assert!(!html.contains("<header"), "no page chrome");
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
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
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
        component_id: Some(Uuid::nil()),
        component_name: "Gateway".into(),
        title: "Latency spike".into(),
        started_at: Utc::now() - ChronoDuration::minutes(14),
        ended_at: None,
        severity: IncidentSeverity::Major,
        impact: IncidentImpact::MajorOutage,
        status_phase: IncidentStatusPhase::Identified,
        updates: vec![PublicIncidentUpdate {
            posted_at: Utc::now() - ChronoDuration::minutes(2),
            phase: IncidentStatusPhase::Identified,
            message: "Rolling back the deploy.".into(),
            generated: false,
        }],
        postmortem: None,
        recovered: Vec::new(),
        recovering: false,
    });
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
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
        updates: vec![
            PublicMaintenanceUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(4),
                phase: MaintenanceUpdatePhase::InProgress,
                message: "Started on time.".into(),
            },
            PublicMaintenanceUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(1),
                phase: MaintenanceUpdatePhase::InProgress,
                message: "Running late, done by 23:30 UTC.".into(),
            },
        ],
        id: Uuid::nil(),
        title: "DB upgrade".into(),
        description: Some("Brief".into()),
        starts_at: Utc::now() - ChronoDuration::minutes(5),
        ends_at: Utc::now() + ChronoDuration::hours(1),
        affected_component_names: vec!["Gateway".into()],
    });
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Scheduled maintenance"));
    assert!(html.contains("DB upgrade"));
    assert!(html.contains("Gateway"));
    assert!(html.contains("Latest update"));
    assert!(html.contains("Running late, done by 23:30 UTC."));
    assert!(!html.contains("Started on time."), "only the newest update");
    assert!(
        html.contains(r#"data-tz="window""#),
        "a window's times name their zone"
    );
}

#[test]
fn a_maintenance_day_links_to_the_window_in_the_archive() {
    let mut p = sample_page();
    p.generated_at = p
        .generated_at
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let comp_id = p.groups[0].components[0].id;
    let last = p.groups[0].components[0].history.len() - 1;
    p.groups[0].components[0].history[last] = DayState::Maintenance;
    let id = Uuid::from_u128(7);
    let started = p.generated_at - ChronoDuration::hours(4);
    // The monitor left the window and came back: two stretches, one day.
    let stretch = |from: i64, until: i64| HistoryMarker {
        kind: MarkerKind::Maintenance { starts_at: started },
        id,
        component_id: comp_id,
        title: "Maintenance: Database upgrade".into(),
        started_at: p.generated_at - ChronoDuration::minutes(from),
        ended_at: Some(p.generated_at - ChronoDuration::minutes(until)),
    };
    let markers = [stretch(150, 120), stretch(90, 60)];
    let view = build_view(&p, &markers, &Default::default(), all_tab(), Tr::default());
    assert_eq!(
        view.groups[0].components[0].history[last].class,
        "day-cell--mnt"
    );
    let blob: serde_json::Value = serde_json::from_str(&view.day_strip_json).unwrap();
    let day = &blob[comp_id.to_string()]["days"][last]["related"];
    assert_eq!(day.as_array().unwrap().len(), 1, "one link per window");
    let related = &day[0];
    assert_eq!(related["title"], "Maintenance: Database upgrade");
    let url = related["url"].as_str().unwrap();
    let (path, anchor) = url.split_once('#').unwrap();
    assert_eq!(anchor, format!("past-{id}"));
    let cursor = path
        .strip_prefix("/status/incidents?show=maintenance&cursor=")
        .unwrap();
    let from = crate::pagination::HistoryCursor::decode(cursor).unwrap();
    assert!(
        (started, id) < (from.started_at, from.id),
        "the archive page starts with the window"
    );
    let tied_after = Uuid::from_u128(id.as_u128() + 1);
    assert!(
        (started, tied_after) >= (from.started_at, from.id),
        "a window that started with it and sorts above it is on an earlier page"
    );
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
fn every_status_label_has_a_message_in_every_locale() {
    let mut ids = Vec::new();
    for s in [
        OverallState::Operational,
        OverallState::Maintenance,
        OverallState::MinorDisruption,
        OverallState::PartialOutage,
        OverallState::MajorOutage,
    ] {
        let (_, _, aria) = overall_classes(s);
        ids.extend([
            crate::public_status::overall_status::overall_label_id(s),
            aria,
        ]);
    }
    for s in [
        PublicComponentStatus::Operational,
        PublicComponentStatus::Degraded,
        PublicComponentStatus::PartialOutage,
        PublicComponentStatus::MajorOutage,
        PublicComponentStatus::Maintenance,
        PublicComponentStatus::NoData,
    ] {
        ids.push(component_classes(s).0);
    }
    for s in [
        DayState::Operational,
        DayState::Degraded,
        DayState::PartialOutage,
        DayState::MajorOutage,
        DayState::Maintenance,
        DayState::NoData,
    ] {
        ids.push(day_classes(s).1);
    }
    for i in [
        IncidentImpact::Degraded,
        IncidentImpact::PartialOutage,
        IncidentImpact::MajorOutage,
    ] {
        ids.push(impact_classes(i).0);
    }
    for p in [
        IncidentStatusPhase::Investigating,
        IncidentStatusPhase::Identified,
        IncidentStatusPhase::Monitoring,
        IncidentStatusPhase::Resolved,
        IncidentStatusPhase::Postmortem,
    ] {
        ids.push(phase_classes(p).0);
    }
    for locale in crate::domain::Locale::ALL {
        for id in &ids {
            assert_ne!(Tr::new(locale).t(id), *id, "{locale:?} has no {id}");
        }
    }
}

#[test]
fn german_page_renders_german_chrome() {
    let tr = Tr::new(crate::domain::Locale::De);
    let html = StatusFullPage {
        reader: Reader::default(),
        tr,
        view: build_view(&sample_page(), &[], &Default::default(), all_tab(), tr),
        branding: BrandingView::sample(PublicOrgBranding::default()),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(
        html.contains(r#"<html lang="de" data-time-locale="de">"#),
        "{html}"
    );
    assert!(html.contains(r#"<meta property="og:locale" content="de_DE">"#));
    assert!(html.contains("Updates abonnieren"));
    assert!(html.contains("Verfügbarkeit"));
    assert!(!html.contains("Subscribe to updates"));
    assert!(!html.contains("Last checked"));
    assert!(!html.contains('\u{2068}'));
}

#[test]
fn german_summary_counts_days_and_outages_in_german() {
    let tr = Tr::new(crate::domain::Locale::De);
    let mut h = vec![DayState::Operational; 90];
    h[10] = DayState::MajorOutage;
    assert_eq!(
        history_summary(&h, &[], tr),
        "90 Tage, 1 Ausfall, 0 beeinträchtigt"
    );
    assert_eq!(
        history_summary(&[DayState::Operational], &[], tr),
        "1 Tag, keine Störungen"
    );
}

#[test]
fn history_summary_tallies_painted_days() {
    let mut h = vec![DayState::Operational; 90];
    h[10] = DayState::MajorOutage;
    assert_eq!(
        history_summary(&h, &[], Tr::default()),
        "90 days, 1 outage, 0 degraded"
    );
    assert_eq!(
        history_summary(&[DayState::NoData; 90], &[], Tr::default()),
        "90 days, no data"
    );
    let mut planned = vec![DayState::NoData; 90];
    planned[40] = DayState::Maintenance;
    planned[41] = DayState::Maintenance;
    assert_eq!(
        history_summary(&planned, &[], Tr::default()),
        "90 days, no data",
        "a window is no evidence of health"
    );
    let mut checked = vec![false; 90];
    checked[40] = true;
    checked[41] = true;
    assert_eq!(
        history_summary(&planned, &checked, Tr::default()),
        "2 days, no incidents",
        "checked through the window, its days count"
    );
    checked[41] = false;
    planned[50] = DayState::Operational;
    assert_eq!(
        history_summary(&planned, &checked, Tr::default()),
        "2 days, no incidents",
        "an unchecked maintenance day does not"
    );
}

#[test]
fn component_uptime_is_the_aggregator_figure_not_a_day_ratio() {
    let mut c = sample_page().groups[0].components[0].clone();
    c.history[10] = DayState::MajorOutage;
    c.uptime_pct = Some(99.972);
    let view = build_component(
        &c,
        &Default::default(),
        &DayText::new(Tr::default(), Utc::now(), 90),
    );
    assert_eq!(view.uptime_label, "99.97%");
    c.uptime_pct = None;
    assert_eq!(
        build_component(
            &c,
            &Default::default(),
            &DayText::new(Tr::default(), Utc::now(), 90)
        )
        .uptime_label,
        "—"
    );
}

#[test]
fn incident_detail_renders() {
    let inc = PublicIncident {
        id: Uuid::nil(),
        component_id: Some(Uuid::nil()),
        component_name: "Gateway".into(),
        title: "Latency spike".into(),
        started_at: Utc::now() - ChronoDuration::minutes(30),
        ended_at: Some(Utc::now()),
        severity: IncidentSeverity::Major,
        impact: IncidentImpact::MajorOutage,
        status_phase: IncidentStatusPhase::Resolved,
        updates: vec![
            PublicIncidentUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(25),
                phase: IncidentStatusPhase::Investigating,
                message: "Looking into it.".into(),
                generated: false,
            },
            PublicIncidentUpdate {
                posted_at: Utc::now() - ChronoDuration::minutes(5),
                phase: IncidentStatusPhase::Resolved,
                message: "Rolled back the deploy.".into(),
                generated: false,
            },
        ],
        postmortem: None,
        recovered: Vec::new(),
        recovering: false,
    };
    let detail = IncidentDetailView::from_incident(&inc, Utc::now(), Tr::default());
    let html = IncidentDetailPage {
        tr: Tr::default(),
        branding: BrandingView::sample(PublicOrgBranding::default()),
        incident: detail,
        generated_at: Utc::now(),
        reader: Reader::default(),
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
fn a_recovering_incident_reads_as_monitoring_without_the_ongoing_badge() {
    let mut inc = fake_incident(Utc::now() - ChronoDuration::minutes(20), 7, "API down");
    inc.ended_at = None;
    let page = |inc: &PublicIncident| {
        IncidentDetailPage {
            tr: Tr::default(),
            branding: BrandingView::sample(PublicOrgBranding::default()),
            incident: IncidentDetailView::from_incident(inc, Utc::now(), Tr::default()),
            generated_at: Utc::now(),
            reader: Reader::default(),
            og: OgMeta::default(),
        }
        .render()
        .unwrap()
    };
    assert!(
        page(&inc).contains("status-badge--down"),
        "down and ongoing"
    );

    inc.status_phase = IncidentStatusPhase::Investigating.shown(true);
    inc.recovering = true;
    let html = page(&inc);
    assert!(!html.contains("status-badge--down"), "{html}");
    assert!(html.contains("public-phase--monitoring"));
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
        tr: Tr::default(),
        branding: BrandingView::sample(PublicOrgBranding::default()),
        incident: IncidentDetailView::from_incident(&inc, Utc::now(), Tr::default()),
        generated_at: Utc::now(),
        reader: Reader::default(),
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
        tr: Tr::default(),
        branding: BrandingView::sample(PublicOrgBranding::default()),
        incident: IncidentDetailView::from_incident(&plain, Utc::now(), Tr::default()),
        generated_at: Utc::now(),
        reader: Reader::default(),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(!html2.contains("Postmortem"));
}

#[test]
fn branding_renders_logo_about_and_color() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let branding = BrandingView::sample(PublicOrgBranding {
        public_display_name: Some("Acme Public".into()),
        public_about: Some("**hi** there".into()),
        public_brand_color: Some("#ff0000".into()),
        logo_hash: Some("deadbeefcafef00d".into()),
        public_show_powered_by: Some(false),
        ..PublicOrgBranding::default()
    });
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
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
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
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
        incident_description("Checkout 500s", "API", "Acme", Tr::default())
            .starts_with("Checkout 500s, affecting API:")
    );
    assert!(
        incident_description("Checkout 500s", "", "Acme", Tr::default())
            .starts_with("Checkout 500s: current")
    );
}

#[test]
fn incident_description_names_the_page_once() {
    assert!(
        incident_description("Outage", "", "Acme", Tr::default())
            .ends_with("on the Acme Status page.")
    );
    assert!(
        incident_description("Outage", "", "Acme Status", Tr::default())
            .ends_with("on the Acme Status page.")
    );
    let long = incident_description(
        "Outage",
        "",
        "Northwind Trading Co. Status Page",
        Tr::default(),
    );
    assert!(long.ends_with(" Status page."), "{long}");
    let ends_in_status = incident_description(
        "Outage",
        "",
        "Northwind Trading Company Status",
        Tr::default(),
    );
    assert!(
        ends_in_status.ends_with("on the Northwind Trading Company Status page."),
        "{ends_in_status}"
    );
    let longer = incident_description(
        "Outage",
        "",
        "Northwind Trading Company Europe Status",
        Tr::default(),
    );
    assert!(longer.ends_with(" Status page."), "{longer}");
}

#[test]
fn incident_descriptions_differ_per_incident() {
    assert_ne!(
        incident_description("Checkout 500s", "API", "Acme", Tr::default()),
        incident_description("Latency spike", "API", "Acme", Tr::default())
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
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
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
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains("Powered by"));
    assert!(
        html.contains(r#"<a href="https://uptimepage.dev/?utm_source=status-page&amp;utm_medium=powered-by" class="hover:text-body">uptimepage</a>"#)
    );
}

#[test]
fn footer_source_links_follow_their_flag() {
    let branding = |show_source_links| BrandingView {
        show_source_links,
        ..BrandingView::sample(PublicOrgBranding::default())
    };
    let status = |b: BrandingView| {
        StatusFullPage {
            reader: Reader::default(),
            tr: Tr::default(),
            view: build_view(
                &sample_page(),
                &[],
                &Default::default(),
                all_tab(),
                Tr::default(),
            ),
            branding: b,
            og: OgMeta::default(),
        }
        .render()
        .unwrap()
    };
    let incident = |b: BrandingView| {
        let inc = fake_incident(Utc::now() - ChronoDuration::hours(2), 8, "Blip");
        IncidentDetailPage {
            tr: Tr::default(),
            branding: b,
            incident: IncidentDetailView::from_incident(&inc, Utc::now(), Tr::default()),
            generated_at: Utc::now(),
            reader: Reader::default(),
            og: OgMeta::default(),
        }
        .render()
        .unwrap()
    };
    for render in [&status as &dyn Fn(BrandingView) -> String, &incident] {
        let html = render(branding(true));
        assert!(html.contains(">Licenses</a>"));
        assert!(html.contains("Source code (AGPL-3.0)"));
        let html = render(branding(false));
        assert!(!html.contains(">Licenses</a>"));
        assert!(!html.contains("Source code (AGPL-3.0)"));
    }
}

// PRE-MORTEM PM #6: a relaxed DB/app validator must not let a crafted
// brand colour break out of the `:root` rule. The template-side
// sanitiser is the independent layer that holds even then.
#[test]
fn malicious_brand_color_cannot_escape_style_rule() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let branding = BrandingView::sample(PublicOrgBranding {
        public_brand_color: Some("red; } body { display: none } /*".into()),
        ..PublicOrgBranding::default()
    });
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
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
        component_id: Some(Uuid::nil()),
        component_name: "API".into(),
        title: title.into(),
        started_at,
        ended_at: Some(started_at + ChronoDuration::minutes(15)),
        severity: IncidentSeverity::Minor,
        impact: IncidentImpact::Degraded,
        status_phase: IncidentStatusPhase::Resolved,
        updates: Vec::new(),
        postmortem: None,
        recovered: Vec::new(),
        recovering: false,
    }
}

#[test]
fn a_long_update_is_cut_by_characters_not_bytes() {
    let mut inc = fake_incident(Utc::now(), 1, "Störung");
    inc.updates.push(PublicIncidentUpdate {
        posted_at: Utc::now(),
        phase: IncidentStatusPhase::Identified,
        message: format!("{}ü{}", "a".repeat(239), "b".repeat(20)),
        generated: false,
    });
    let summary = build_incident_summary(&inc, Utc::now(), Tr::default());
    assert_eq!(
        summary.latest_message.as_deref(),
        Some(format!("{}ü…", "a".repeat(239)).as_str())
    );
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
    let window = PublicMaintenance {
        updates: Vec::new(),
        id: Uuid::now_v7(),
        title: "Database upgrade".into(),
        description: None,
        starts_at: may_b + ChronoDuration::days(3),
        ends_at: may_b + ChronoDuration::days(3) + ChronoDuration::hours(1),
        affected_component_names: vec!["API".into()],
    };
    let items = vec![
        HistoryEntry::Incident(fake_incident(may_a, 1, "May late")),
        HistoryEntry::Maintenance(window),
        HistoryEntry::Incident(fake_incident(may_b, 2, "May early")),
        HistoryEntry::Incident(fake_incident(apr, 3, "April")),
    ];
    let buckets = bucket_by_month(items, now, Tr::default());
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].label, "May 2026");
    let may: Vec<&str> = buckets[0].rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(may, ["May late", "Database upgrade", "May early"]);
    assert_eq!(buckets[1].label, "April 2026");
    assert_eq!(buckets[1].rows.len(), 1);
}

fn archive_page(
    tab: HistoryTab,
    months: Vec<MonthBucket>,
    cursor: Option<&str>,
    robots: &'static str,
) -> IncidentArchivePage {
    let tr = Tr::default();
    IncidentArchivePage {
        tr,
        branding: BrandingView::sample(PublicOrgBranding::default()),
        heading: tab.t(tr, "archive-heading"),
        empty: tab.t(tr, "archive-empty"),
        tabs: super::past::tab_links(ARCHIVE_URL, "", tab, tr),
        months,
        older_url: cursor.map(|c| archive_path(tab, Some(c))),
        reader: Reader::default(),
        robots,
        og: OgMeta::default(),
    }
}

#[test]
fn archive_page_renders_buckets_and_next_link() {
    let now = Utc::now();
    let started = chrono::DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let items = vec![HistoryEntry::Incident(fake_incident(
        started,
        1,
        "ECS rolled back",
    ))];
    let months = bucket_by_month(items, now, Tr::default());
    let html = archive_page(
        HistoryTab::Incidents,
        months,
        Some("opaque-cursor-token"),
        "noindex,follow",
    )
    .render()
    .unwrap();
    assert!(html.contains("<title>Incident history · "));
    assert!(html.contains("May 2026"));
    assert!(html.contains("ECS rolled back"));
    assert!(
        html.contains("/status/incidents?show=incidents&#38;cursor=opaque-cursor-token"),
        "the next page stays on the tab"
    );
    assert!(
        html.contains(
            r#"<a href="/status/incidents" id="history-tab-1" class="history-tab">All</a>"#
        )
    );
    assert!(html.contains(
        r#"<a href="/status/incidents?show=incidents" id="history-tab-2" class="history-tab" aria-current="page">Incidents</a>"#
    ));
}

#[test]
fn archive_path_keeps_the_tab_and_the_cursor() {
    assert_eq!(archive_path(HistoryTab::All, None), "/status/incidents");
    assert_eq!(
        archive_path(HistoryTab::All, Some("c")),
        "/status/incidents?cursor=c"
    );
    assert_eq!(
        archive_path(HistoryTab::Maintenance, Some("c")),
        "/status/incidents?show=maintenance&cursor=c"
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
fn only_the_cursorless_archive_page_on_the_default_tab_is_indexable() {
    let branding = BrandingView::sample(PublicOrgBranding::default());
    assert_eq!(
        archive_robots(HistoryTab::All, None, &branding),
        "index,follow"
    );
    assert_eq!(
        archive_robots(HistoryTab::All, Some("opaque-cursor-token"), &branding),
        "noindex,follow"
    );
    assert_eq!(
        archive_robots(HistoryTab::Maintenance, None, &branding),
        "noindex,follow"
    );
}

#[test]
fn only_a_plan_that_sells_white_label_follows_the_operators_link() {
    let page = |follow: bool| {
        let mut branding = BrandingView::sample(PublicOrgBranding {
            public_website_url: Some("https://acme.example".into()),
            ..Default::default()
        });
        branding.follow_website_link = follow;
        StatusFullPage {
            reader: Reader::default(),
            tr: Tr::default(),
            view: build_view(
                &sample_page(),
                &[],
                &Default::default(),
                all_tab(),
                Tr::default(),
            ),
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
fn the_header_brand_falls_back_to_the_status_page_root() {
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view: build_view(
            &sample_page(),
            &[],
            &Default::default(),
            all_tab(),
            Tr::default(),
        ),
        branding: BrandingView::sample(PublicOrgBranding::default()),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<a href="/""#));
    assert!(!html.contains("nofollow"), "an internal link takes no rel");
}

#[test]
fn hiding_a_page_from_search_takes_every_page_with_it() {
    let branding = BrandingView::sample(PublicOrgBranding {
        public_hide_from_search: true,
        ..Default::default()
    });
    assert_eq!(branding.robots(), "noindex,follow");
    assert_eq!(
        archive_robots(HistoryTab::All, None, &branding),
        "noindex,follow"
    );

    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view: build_view(
            &sample_page(),
            &[],
            &Default::default(),
            all_tab(),
            Tr::default(),
        ),
        branding,
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<meta name="robots" content="noindex,follow">"#));
}

#[test]
fn archive_page_renders_empty_state_without_next_link() {
    let html = archive_page(HistoryTab::Maintenance, Vec::new(), None, "noindex,follow")
        .render()
        .unwrap();
    assert!(html.contains("No maintenance recorded."));
    assert!(html.contains("Maintenance history"));
    assert!(!html.contains("Older entries"));
    assert!(html.contains(r#"<meta name="robots" content="noindex,follow">"#));
}

#[test]
fn branding_defaults_when_all_fields_null() {
    // With every override unset, the display name falls back to the org
    // name and no logo image is emitted (the header shows text). The
    // default colour and powered-by footer are covered by their own
    // tests above; this one pins the resolved-name + no-logo path.
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let branding = BrandingView::sample(PublicOrgBranding::default());
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
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

#[test]
fn past_incidents_render_as_rows_with_older_ones_folded_away() {
    let mut p = sample_page();
    let now = p.generated_at;
    // Two blips on one component twenty minutes apart, and one from last month.
    p.recent_incidents = vec![
        fake_incident(now - ChronoDuration::hours(2), 1, "API degraded"),
        fake_incident(now - ChronoDuration::minutes(155), 2, "API degraded again"),
        fake_incident(now - ChronoDuration::days(20), 3, "API slow"),
    ];
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains("History (30 days)"));
    let folded = format!(
        r#"<details class="past-fold" id="past-{}" data-keep-open>"#,
        Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])
    );
    assert!(html.contains(&folded), "the two blips share one row");
    assert!(html.contains("2 incidents"));
    assert!(html.contains(r#"<details class="past-earlier" id="past-earlier" data-keep-open>"#));
    assert!(html.contains("1 earlier entry"));
    assert!(html.contains(r#"data-tz="day-time""#));
    assert!(!html.contains("No resolved incidents or completed maintenance in the past 7 days."));

    p.recent_incidents.truncate(0);
    p.recent_incidents
        .push(fake_incident(now - ChronoDuration::days(20), 3, "API slow"));
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains("No resolved incidents or completed maintenance in the past 7 days."));
    assert!(!html.contains(r#"class="past-fold"#));
}

#[test]
fn a_page_with_no_history_still_shows_the_section_and_the_archive_link() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"<section id="history""#));
    assert!(html.contains("History (30 days)"));
    assert!(html.contains("No resolved incidents or completed maintenance in the past 30 days."));
    assert!(html.contains(r#"<a href="/status/incidents" class="font-medium"#));
    assert!(html.contains(
        r#"<a href="/#history" id="history-tab-1" class="history-tab" aria-current="page">All</a>"#
    ));
}

#[test]
fn a_tab_keeps_itself_through_the_refresh_and_into_the_archive() {
    let mut p = sample_page();
    let now = p.generated_at;
    p.recent_incidents = vec![fake_incident(
        now - ChronoDuration::hours(2),
        1,
        "API degraded",
    )];
    p.recent_maintenance = vec![PublicMaintenance {
        updates: Vec::new(),
        id: Uuid::from_u128(9),
        title: "Database upgrade".into(),
        description: Some("Primary moves to new hardware.".into()),
        starts_at: now - ChronoDuration::hours(5),
        ends_at: now - ChronoDuration::hours(4),
        affected_component_names: vec!["Gateway".into()],
    }];
    let history = HistoryRequest {
        tab: HistoryTab::Maintenance,
        home: "/status",
    };
    let view = build_view(&p, &[], &Default::default(), history, Tr::default());
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"hx-get="?fragment=1&#38;show=maintenance""#));
    assert!(html.contains(r#"href="/status/incidents?show=maintenance""#));
    assert!(html.contains(
        r#"<a href="/status?show=maintenance#history" id="history-tab-3" class="history-tab" aria-current="page">Maintenance</a>"#
    ));
    assert!(html.contains("Database upgrade"));
    assert!(html.contains(r#"<span class="public-chip public-sev--mnt">Maintenance</span>"#));
    assert!(html.contains("Primary moves to new hardware."));
    assert!(
        !html.contains("API degraded"),
        "incidents are on their own tab"
    );

    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    assert!(html.contains("Database upgrade") && html.contains("API degraded"));
    assert!(html.contains(r#"hx-get="?fragment=1""#));
}

#[test]
fn a_long_maintenance_description_opens_in_place_and_hides_its_preview() {
    let mut p = sample_page();
    let now = p.generated_at;
    let full = "Traffic moves to the standby links. ".repeat(12);
    p.recent_maintenance = vec![PublicMaintenance {
        updates: Vec::new(),
        id: Uuid::from_u128(9),
        title: "Network maintenance".into(),
        description: Some(full.clone()),
        starts_at: now - ChronoDuration::hours(5),
        ends_at: now - ChronoDuration::hours(4),
        affected_component_names: vec!["Gateway".into()],
    }];
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let html = StatusRegion {
        tr: Tr::default(),
        view,
    }
    .render()
    .unwrap();
    let class_of = |text: &str| {
        let tag = &html[..html.find(text).unwrap()];
        let start = tag.rfind("class=\"").unwrap() + 7;
        tag[start..start + tag[start..].find('"').unwrap()].to_owned()
    };
    assert!(
        class_of(r#" id="past-"#)
            .split(' ')
            .any(|c| c == "group/fold"),
        "the long description opens in place"
    );
    assert!(
        class_of(">Traffic moves")
            .split(' ')
            .any(|c| c == "group-open/fold:hidden"),
        "the preview hides itself once the row is open"
    );
    assert!(
        html.contains(full.trim_end()),
        "the open row shows all of it"
    );
}

#[test]
fn the_status_page_loads_the_script_that_keeps_rows_open() {
    let view = build_view(
        &sample_page(),
        &[],
        &Default::default(),
        all_tab(),
        Tr::default(),
    );
    let html = StatusFullPage {
        reader: Reader::default(),
        tr: Tr::default(),
        view,
        branding: BrandingView::sample(PublicOrgBranding::default()),
        og: OgMeta::default(),
    }
    .render()
    .unwrap();
    assert!(html.contains(&crate::templates::assets::url("js/public/keep_open.js")));
}

#[test]
fn a_day_touched_for_no_measurable_time_still_names_its_state() {
    let mut p = sample_page();
    let comp_id = p.groups[0].components[0].id;
    let last = p.groups[0].components[0].history.len() - 1;
    p.groups[0].components[0].history[last] = DayState::PartialOutage;
    let view = build_view(&p, &[], &Default::default(), all_tab(), Tr::default());
    let cell = &view.groups[0].components[0].history[last];
    assert_eq!(
        cell.class, "day-cell--deg",
        "off green, at the lightest tint"
    );
    assert!(
        cell.aria_label.ends_with("— Partial outage"),
        "{}",
        cell.aria_label
    );
    let blob: serde_json::Value = serde_json::from_str(&view.day_strip_json).unwrap();
    assert_eq!(
        blob[comp_id.to_string()]["days"][last]["spans"],
        serde_json::json!([
            { "state": "Partial outage", "state_class": "day-pop-status--part", "duration": "" },
        ])
    );
}
