use super::fields::*;
use super::from_target::{FormKind, empty_create_form, form_from_target, http_fields_from};
use super::model::*;
use super::prefill::{apply_kind_param, prefill_host, prefill_url};
use super::*;

use crate::domain::REDACTED;
use crate::domain::{CheckSpec, ExpectedStatus, HttpMethod, Target};

/// The chip that says a channel already covers this monitor is decided in the
/// browser from the tags being edited, so the rule has to reach the markup.
#[test]
fn a_channels_tag_rule_reaches_the_form_for_the_client_to_match_on() {
    let mut form = empty_create_form();
    form.channels = vec![ChannelChoice {
        id: "c1".into(),
        name: "db team".into(),
        kind: "slack",
        selected: false,
        rule_tags: r#"["db","us east"]"#.into(),
    }];
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    // Escaped by the template, so the browser hands JSON.parse the real tags.
    assert!(html.contains(r#"data-rule-tags="[&#34;db&#34;,&#34;us east&#34;]""#));
    assert!(html.contains("data-rule-chip"));
}

#[test]
fn new_form_renders_empty_create() {
    let page = FormPage {
        active_tab: "targets",
        form: empty_create_form(),
    };
    let html = page.render().unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("New monitor"));
    assert!(html.contains(r#"data-action="/api/v1/targets""#));
    assert!(html.contains(r#"data-method="POST""#));
    assert!(html.contains(r#"data-mode="create""#));
    assert!(html.contains(r#"name="check_type" value="http""#));
}

#[test]
fn a_kind_param_preselects_the_rail() {
    let mut form = empty_create_form();
    assert!(apply_kind_param(&mut form, "dns"));
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="check_type" value="dns" checked"#));

    // A hand-edited kind is ignored rather than silently landing on http.
    let mut form = empty_create_form();
    assert!(!apply_kind_param(&mut form, "nonsense"));
    assert_eq!(form.check_type, "http");
}

#[test]
fn a_url_param_prefills_the_field_and_names_the_monitor() {
    let mut form = empty_create_form();
    prefill_url(&mut form, "acme.com/health");
    assert_eq!(form.http.url, "https://acme.com/health");
    assert_eq!(form.name, "acme.com");

    // Every other kind is left alone, flow included — the plan can still
    // downgrade it to http and a flow-field URL would be lost.
    for kind in ["flow", "ping"] {
        let mut form = empty_create_form();
        assert!(apply_kind_param(&mut form, kind));
        prefill_url(&mut form, "acme.com");
        assert!(
            form.flow.start_url.is_empty() && form.ping.host.is_empty(),
            "{kind}"
        );
    }
    for raw in ["", "javascript:alert(1)", "https://"] {
        let mut form = empty_create_form();
        prefill_url(&mut form, raw);
        assert!(form.http.url.is_empty(), "{raw}");
    }
}

#[test]
fn edit_locks_the_kind_to_a_static_rail() {
    let mut form = empty_create_form();
    form.mode = "edit";
    form.check_type = "tcp";
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(!html.contains("data-check-card"), "no selectable cards");
    assert!(html.contains(r#"<input type="hidden" name="check_type" value="tcp">"#));
    assert!(html.contains("check-type-card--static check-type-card--on"));
}

#[test]
fn flow_is_offered_but_locked_until_the_plan_allows_it() {
    let html = FormPage {
        active_tab: "targets",
        form: empty_create_form(),
    }
    .render()
    .unwrap();
    assert!(html.contains("browser login / journey"));
    assert!(!html.contains(r#"name="check_type" value="flow""#));
    assert!(html.contains(r#"<span class="card-badge card-badge--warn">coming soon</span>"#));

    let mut form = empty_create_form();
    form.flow_available = true;
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="check_type" value="flow""#));
    assert!(!html.contains("coming soon"));
    assert_eq!(html.matches(r#"card-badge--ok">new<"#).count(), 2);

    // The URL names the kind before the plan is known; it stays locked.
    let mut form = empty_create_form();
    assert!(apply_kind_param(&mut form, "flow"));
    assert!(
        form.kind_cards()
            .iter()
            .any(|c| c.value == "flow" && c.locked),
        "a plan without flow keeps the card locked whatever the URL asked for"
    );
}

#[test]
fn test_results_render_into_a_docked_sheet() {
    let html = FormPage {
        active_tab: "targets",
        form: empty_create_form(),
    }
    .render()
    .unwrap();
    assert!(html.contains("data-test-sheet"));
    assert!(html.contains("data-test-verdict"));
    assert!(html.contains(r#"<div id="test-sheet-body" class="test-sheet__body" hidden>"#));
    assert!(html.contains("data-test-result"));
}

#[test]
fn heartbeat_is_offered_to_new_monitors() {
    let html = FormPage {
        active_tab: "targets",
        form: empty_create_form(),
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="check_type" value="heartbeat""#));
    assert!(html.contains(r#"<span class="card-badge card-badge--ok">new</span>"#));
}

#[test]
fn renders_a_card_and_panel_for_every_check_kind() {
    let html = FormPage {
        active_tab: "targets",
        form: empty_create_form(),
    }
    .render()
    .unwrap();
    // Flow is the only kind that can lock, and a locked card has no radio.
    let selectable = CheckSpec::ALL_KINDS.iter().filter(|k| **k != "flow");
    assert_eq!(
        html.matches("data-check-card").count(),
        selectable.clone().count()
    );
    for v in selectable {
        assert!(
            html.contains(&format!(r#"value="{v}""#)),
            "missing card for variant {v}"
        );
    }
    for v in CheckSpec::ALL_KINDS {
        // Each protocol panel is rendered (non-active panels are .hidden).
        assert!(
            html.contains(&format!(r#"data-variant="{v}""#)),
            "missing panel for variant {v}"
        );
    }
    assert!(html.contains(r#"name="check_type" value="http" checked"#));
}

#[test]
fn smart_expected_status_round_trips_all_three_shapes() {
    for (status, want) in [
        (ExpectedStatus::Exact(204), "204"),
        (ExpectedStatus::Range { min: 200, max: 299 }, "200-299"),
        (ExpectedStatus::OneOf(vec![200, 201, 204]), "200, 201, 204"),
    ] {
        use crate::domain::HttpCheck;
        use std::collections::HashMap;
        use std::time::Duration;
        use url::Url;
        let h = HttpCheck {
            url: Url::parse("https://example.com").unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_millis(5_000),
            follow_redirects: true,
            max_redirects: 5,
            expected_status: status,
            expected_body_contains: None,
            headers: HashMap::new(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        };
        assert_eq!(http_fields_from(h).expected_status_input, want);
    }
}

#[test]
fn headers_render_as_row_inputs() {
    use crate::domain::HttpCheck;
    use std::collections::HashMap;
    use std::time::Duration;
    use url::Url;
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("X-Request-Id".to_string(), "abc".to_string());
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "api".into(),
        check: CheckSpec::Http(HttpCheck {
            url: Url::parse("https://example.com").unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_millis(5_000),
            follow_redirects: true,
            max_redirects: 5,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers,
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.http.headers.len(), 2);
    // Sorted alphabetically by name for stable rendering.
    assert_eq!(form.http.headers[0].name, "Accept");
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    // Container has `data-header-rows` (plural); each row has `data-header-row`
    // (no value, followed by class). Count the row attr with trailing space
    // to avoid matching the container, and split off the clone <template>,
    // which holds a blank row of the same shape.
    let (rows, tmpl) = html
        .split_once(r#"<template id="header-row-template""#)
        .expect("header-row-template must exist; the JS repeater clones it");
    assert_eq!(rows.matches("data-header-row ").count(), 2);
    assert_eq!(tmpl.matches("data-header-row ").count(), 1);
    assert!(html.contains(r#"name="http_header_key""#));
    assert!(html.contains(r#"name="http_header_value""#));
    assert!(html.contains(r#"value="Accept""#));
    assert!(html.contains(r#"value="application/json""#));
}

#[test]
fn tags_render_as_chips() {
    let mut form = empty_create_form();
    form.tags = vec!["prod".into(), "api".into()];
    // The monitor's tags must be in the option list to render (the handler
    // guarantees this via `ensure_tags_listed`); here we set it directly.
    form.tag_options = vec!["prod".into(), "api".into(), "staging".into()];
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    // One pick chip per option; the assigned two are pre-checked.
    assert_eq!(html.matches("data-tag-pick").count(), 3);
    assert!(html.contains(r#"value="prod" class="sr-only" checked"#));
    assert!(html.contains(r#"value="api" class="sr-only" checked"#));
    assert!(html.contains(r#"value="staging" class="sr-only">"#));
}

#[test]
fn region_groups_bucket_by_continent_other_last() {
    let region = |id: &str, cont: Option<&str>| crate::storage::RegionOption {
        id: id.into(),
        name: id.into(),
        city: String::new(),
        country_code: None,
        continent: cont.map(str::to_string),
        latitude: None,
        longitude: None,
    };
    let available = vec![
        region("us", Some("north_america")),
        region("eu", Some("europe")),
        region("mystery", None),
    ];
    let groups = region_groups(
        available,
        |id| id == "eu",
        &std::collections::HashSet::new(),
    );
    let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
    // Continent::ALL order (Europe before North America), unknown → Other last.
    assert_eq!(labels, vec!["Europe", "North America", "Other"]);
    let eu = &groups[0].regions[0];
    assert_eq!(eu.id, "eu");
    assert!(eu.selected);
    assert!(!groups[2].regions[0].selected);
}

#[test]
fn form_surfaces_plan_interval_floor_and_presets() {
    let mut form = empty_create_form();
    form.min_interval_s = 60;
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    // Client mirror of the API floor (no hardcoded 60 in the markup).
    assert!(html.contains(r#"data-min-interval="60""#));
    // The plan floor drops the 30s preset from the fast rail.
    assert!(!html.contains(r#"name="interval_s" value="30""#));
    assert!(html.contains(r#"name="interval_s" value="60" class="sr-only" checked"#));
    // Both cadence rails render; the slow one starts hidden on http.
    assert!(html.contains(r#"data-interval-rail="fast""#));
    assert!(html.contains(r#"data-interval-rail="slow" hidden"#));
    assert!(html.contains(r#"name="interval_s" value="86400""#));
}

#[test]
fn off_preset_interval_is_preserved_as_an_option() {
    let mut form = empty_create_form();
    form.interval_s = 90;
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="interval_s" value="90" class="sr-only" checked"#));
    assert!(html.contains("90s"));
}

#[test]
fn expiry_kinds_open_on_the_suggested_cadence_not_the_floor() {
    let mut form = empty_create_form();
    assert!(apply_kind_param(&mut form, "domain_expiry"));
    prefill_host(&mut form, "acme.com");
    assert_eq!(form.interval_s, 86_400);
    // Hourly stays legal, it is just not something the picker offers.
    let offered: Vec<u64> = form
        .interval_options_slow()
        .iter()
        .map(|o| o.secs)
        .collect();
    assert_eq!(offered, [43_200, 86_400]);
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="interval_s" value="86400" class="sr-only" checked"#));
}

#[test]
fn a_stored_cadence_below_the_suggestions_survives_an_edit() {
    let mut form = empty_create_form();
    form.check_type = "domain_expiry";
    form.interval_s = 3_600;
    form.interval_pinned = true;
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="interval_s" value="3600" class="sr-only" checked"#));
    // Pinned, so the client leaves it alone.
    assert!(html.contains(r#"data-interval-touched="1""#));
    assert!(html.contains(r#"data-interval="3600""#));
}

#[test]
fn a_preset_duration_selects_its_segment_and_an_odd_one_selects_none() {
    let mut form = empty_create_form();
    form.check_type = "heartbeat";
    form.heartbeat.period_s = 3_600;
    form.heartbeat.grace_s = 45;
    form.heartbeat.max_runtime_s = 0;

    let on: Vec<bool> = form
        .heartbeat
        .period_presets()
        .iter()
        .map(|p| p.selected)
        .collect();
    assert_eq!(on.iter().filter(|s| **s).count(), 1, "1h is a preset");
    assert!(
        form.heartbeat.grace_presets().iter().all(|p| !p.selected),
        "45s is not"
    );
    assert!(
        form.heartbeat.max_runtime_presets()[0].selected,
        "off is a real selection, not an empty box"
    );
    assert_eq!(form.heartbeat.max_runtime_value(), "");

    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"value="1h" data-duration-preset checked"#));
}

#[test]
fn a_heartbeat_period_the_pings_contradict_offers_the_real_one() {
    let mut form = empty_create_form();
    form.check_type = "heartbeat";
    form.heartbeat.period_s = 600;
    form.heartbeat.cadence = Some(CadenceHint {
        observed_s: 4980,
        suggested_s: 5400,
        too_tight: true,
    });
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains("runs less often than you told us"));
    assert!(html.contains("83m"), "what the job really does");
    assert!(html.contains(r#"data-apply-period="90m""#));
}

#[test]
fn a_heartbeat_period_that_fits_says_nothing() {
    let mut form = empty_create_form();
    form.check_type = "heartbeat";
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(!html.contains("data-apply-period"));
    assert!(!html.contains("data-cadence-hint"));
}

#[test]
fn edit_form_renders_target_with_sealed_auth() {
    use crate::domain::HttpCheck;
    use std::collections::HashMap;
    use std::time::Duration;
    use url::Url;

    let t = Target {
        id: uuid::Uuid::nil(),
        name: "api".into(),
        check: CheckSpec::Http(HttpCheck {
            url: Url::parse("https://example.com").unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_millis(5_000),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: HashMap::new(),
            body: None,
            verify_tls: true,
            basic_auth: Some((REDACTED.into(), REDACTED.into())),
            bearer_token: Some(REDACTED.into()),
        }),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.submit_method, "PATCH");
    let page = FormPage {
        active_tab: "targets",
        form,
    };
    let html = page.render().unwrap();
    assert!(html.contains("https://example.com"));
}

#[test]
fn edit_form_maps_tcp_target_fields() {
    use crate::domain::TcpCheck;
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "db".into(),
        check: CheckSpec::Tcp(TcpCheck {
            host: "db.example.com".into(),
            port: 5432,
            timeout: Duration::from_millis(2_500),
        }),
        interval: Duration::from_secs(30),
        enabled: false,
        tags: vec!["prod".into(), "db".into()],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.check_type, "tcp");
    assert_eq!(form.tcp.host, "db.example.com");
    assert_eq!(form.tcp.port, 5432);
    assert_eq!(form.tcp.timeout_ms, 2_500);
    assert_eq!(form.interval_s, 30);
    assert!(!form.enabled);
    assert_eq!(form.tags, vec!["prod".to_string(), "db".to_string()]);
    assert_eq!(form.submit_method, "PATCH");
}

#[test]
fn edit_form_maps_tls_cert_target_fields() {
    use crate::domain::TlsCertCheck;
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "tls".into(),
        check: CheckSpec::TlsCert(TlsCertCheck {
            host: "example.com".into(),
            port: 8443,
            server_name: Some("vhost.example.com".into()),
            warn_days: 14,
            critical_days: 3,
            timeout: Duration::from_millis(4_500),
        }),
        interval: Duration::from_secs(3_600),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.check_type, "tls_cert");
    assert_eq!(form.tls_cert.host, "example.com");
    assert_eq!(form.tls_cert.port, 8443);
    assert_eq!(form.tls_cert.server_name, "vhost.example.com");
    assert_eq!(form.tls_cert.warn_days, 14);
    assert_eq!(form.tls_cert.critical_days, 3);
    assert_eq!(form.tls_cert.timeout_ms, 4_500);
}

#[test]
fn edit_form_maps_domain_expiry_target_fields() {
    use crate::domain::DomainExpiryCheck;
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "dom".into(),
        check: CheckSpec::DomainExpiry(DomainExpiryCheck {
            domain: "example.com".into(),
            warn_days: 60,
            critical_days: 14,
            timeout: Duration::from_secs(10),
        }),
        interval: Duration::from_secs(86_400),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.check_type, "domain_expiry");
    assert_eq!(form.domain_expiry.domain, "example.com");
    assert_eq!(form.domain_expiry.warn_days, 60);
    assert_eq!(form.domain_expiry.critical_days, 14);
    assert_eq!(form.domain_expiry.timeout_ms, 10_000);
}

#[test]
fn edit_form_maps_heartbeat_target_fields() {
    use crate::domain::HeartbeatCheck;
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "nightly backup".into(),
        check: CheckSpec::Heartbeat(HeartbeatCheck {
            period: Duration::from_secs(300),
            grace: Duration::from_secs(45),
            max_runtime: Some(Duration::from_secs(600)),
        }),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.check_type, "heartbeat");
    assert_eq!(form.heartbeat.period_s, 300);
    assert_eq!(form.heartbeat.grace_s, 45);
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    // The schedule rail and test-now are hidden for the passive kind.
    assert!(html.contains("data-schedule-section hidden"));
    assert!(html.contains(r#"name="heartbeat_period_s" type="text" value="5m""#));
    assert!(html.contains(r#"name="heartbeat_grace_s" type="text" value="45s""#));
    // 45s is off-preset, so no grace segment is selected.
    assert!(!html.contains(r#"value="45s" data-duration-preset checked"#));
    // Editing an existing heartbeat still shows its picker card.
    assert!(html.contains(r#"name="check_type" value="heartbeat""#));
}

#[test]
fn edit_form_maps_dns_target_fields() {
    use crate::domain::{DnsCheck, DnsRecordType};
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "dns".into(),
        check: CheckSpec::Dns(DnsCheck {
            domain: "api.example.com".into(),
            record_type: DnsRecordType::Cname,
            resolver: Some("1.1.1.1".into()),
            expected_contains: Some("edge.cdn".into()),
            timeout: Duration::from_millis(2_500),
        }),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Edit).unwrap();
    assert_eq!(form.check_type, "dns");
    assert_eq!(form.dns.domain, "api.example.com");
    assert_eq!(form.dns.record_type, "CNAME");
    assert_eq!(form.dns.resolver, "1.1.1.1");
    assert_eq!(form.dns.expected_contains, "edge.cdn");
    assert_eq!(form.dns.timeout_ms, 2_500);
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"name="check_type" value="dns""#));
    assert!(html.contains(r#"value="CNAME" selected"#));
    assert!(html.contains(r#"value="api.example.com""#));
    assert!(html.contains(r#"value="edge.cdn""#));
    assert!(html.contains(r#"value="1.1.1.1""#));
}

#[test]
fn escalation_selector_is_edit_only() {
    // Create has no monitor id to bind yet → a prompt, no selector.
    let mut create_form = empty_create_form();
    create_form.show_escalation = true;
    let create = FormPage {
        active_tab: "targets",
        form: create_form,
    }
    .render()
    .unwrap();
    assert!(create.contains("Save the monitor first"));
    assert!(!create.contains("data-monitor-policy-select"));

    // Edit renders the binding selector with the inherit option.
    let mut form = empty_create_form();
    form.mode = "edit";
    form.show_escalation = true;
    form.id = "00000000-0000-0000-0000-000000000001".into();
    form.escalation_choices = vec![crate::web::views::escalation::Choice {
        id: "p1".into(),
        name: "Primary".into(),
        selected: true,
    }];
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(html.contains("data-monitor-policy-select"));
    assert!(html.contains(r#"data-target-id="00000000-0000-0000-0000-000000000001""#));
    assert!(html.contains("inherit org default"));
    assert!(html.contains(r#"value="p1" selected"#));
}

#[test]
fn escalation_section_hidden_when_disabled() {
    // show_escalation defaults off → the whole policy block is absent.
    let form = empty_create_form();
    assert!(!form.show_escalation);
    let html = FormPage {
        active_tab: "targets",
        form,
    }
    .render()
    .unwrap();
    assert!(!html.contains("Escalation policy"));
    assert!(!html.contains("Save the monitor first"));
}

#[test]
fn copy_form_seeds_create_from_existing() {
    use crate::domain::TcpCheck;
    use std::time::Duration;
    let t = Target {
        id: uuid::Uuid::nil(),
        name: "db".into(),
        check: CheckSpec::Tcp(TcpCheck {
            host: "db.example.com".into(),
            port: 5432,
            timeout: Duration::from_millis(2_500),
        }),
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec!["prod".into()],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        plan_hold_at: None,
    };
    let form = form_from_target(t, FormKind::Copy).unwrap();
    assert_eq!(form.mode, "create");
    assert_eq!(form.submit_method, "POST");
    assert_eq!(form.action, "/api/v1/targets");
    assert!(form.id.is_empty());
    assert_eq!(form.name, "db (copy)");
    // Check config carried over so the copy is a real duplicate.
    assert_eq!(form.check_type, "tcp");
    assert_eq!(form.tcp.host, "db.example.com");
    assert_eq!(form.tcp.port, 5432);
    assert_eq!(form.tags, vec!["prod".to_string()]);
}

/// The form's heartbeat default has to pass the cadence rule the API applies
/// on submit, or the empty form refuses itself.
#[test]
fn the_heartbeat_default_pairs_with_the_default_interval() {
    let hb = HeartbeatFields::default();
    let spec = CheckSpec::Heartbeat(crate::domain::HeartbeatCheck {
        period: std::time::Duration::from_secs(hb.period_s),
        grace: std::time::Duration::from_secs(hb.grace_s),
        max_runtime: None,
    });
    let interval = crate::domain::interval_hints_for_kind("heartbeat").default;
    crate::api::handlers::targets::validate::validate_heartbeat_cadence(
        &spec,
        std::time::Duration::from_secs(interval),
        60,
    )
    .unwrap();
}
