use super::args::{
    CREDENTIAL_HEADERS, DEFAULT_INCIDENT_WINDOW_DAYS, MAX_INCIDENT_WINDOW_DAYS,
    build_monitor_patch, default_interval_secs, incident_window, new_check_spec,
    parse_expected_status, parse_incident_state_filter, parse_kind, parse_phase,
    parse_region_policy, parse_state, parse_uuid, parse_window, requested_fields, requested_region,
    resolve_bindings,
};
use super::support::deny_terraform;
use super::text::{clean_public_text, create_prompt_lines, sanitize_data, sanitize_prompt};
use super::tools_read::IncidentPage;
use super::view::{
    channel_names, check_config, check_timing, current_state, expected_status_str, flow_run_item,
    incident_detail, incident_summary, ms_to_rfc3339, probe_line, region_cap, region_health,
    region_items, region_policy_view, step_trend_item, ts_to_rfc3339, undeliverable_reason,
};
use super::*;

use std::collections::HashMap;

use chrono::{Duration, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::api::redaction::REDACTED;
use crate::api::types::{DashboardMetrics, FlowStepBucket, FlowStepTrend};
use crate::domain::agent_wire::{ConsoleLine, FlowEvidence, StepOutcome, StepTrace};
use crate::domain::incident::Incident;
use crate::domain::notification_channel::NotificationChannel;
use crate::domain::public::{IncidentSeverity, IncidentStatusPhase, PublicIncidentUpdate};
use crate::domain::result::{CheckResult, CheckStatus};
use crate::domain::target::{NewTarget, RegionIncidentPolicy, Target};
use crate::domain::{AlertBinding, CheckSpec, ExpectedStatus, FlowStep, TargetAlerts, WriteSource};
use crate::mcp::audit::Outcome;
use crate::mcp::cursor;
use crate::mcp::error::{codes, outcome_for, probe_dispatch_error};
use crate::mcp::schema::{
    CheckConfig, FieldChange, MonitorDetail, MonitorUpdateResult, NewCheck, ProbeOutcome,
    RegionPolicyArg, RegionPolicyMode, UpdateMonitorArgs,
};
use crate::storage::TimeRange;
use crate::storage::incidents::IncidentBrief;
use crate::storage::traits::FlowRunView;

fn check_result(dns: Option<u16>, ttfb: Option<u16>, size: Option<u32>) -> CheckResult {
    CheckResult {
        target_id: Uuid::nil(),
        org_id: Uuid::nil(),
        timestamp: Utc::now(),
        status: CheckStatus::Up,
        duration_ms: 100,
        dns_ms: dns,
        connect_ms: Some(30),
        tls_ms: Some(45),
        ttfb_ms: ttfb,
        response_code: Some(200),
        response_size: size,
        diagnostic: None,
        error: None,
    }
}

fn active_incident(latest: Option<PublicIncidentUpdate>) -> IncidentBrief {
    IncidentBrief {
        id: Uuid::nil(),
        target_id: Uuid::nil(),
        target_name: "api".into(),
        severity: IncidentSeverity::Critical,
        started_at: Utc::now(),
        ended_at: None,
        public_title: None,
        latest_update: latest,
    }
}

fn update(phase: IncidentStatusPhase) -> PublicIncidentUpdate {
    PublicIncidentUpdate {
        posted_at: Utc::now(),
        phase,
        message: "msg".into(),
    }
}

#[test]
fn check_timing_copies_phase_fields() {
    let t = check_timing(&check_result(Some(12), Some(120), Some(2048)));
    assert_eq!(t.dns_ms, Some(12));
    assert_eq!(t.connect_ms, Some(30));
    assert_eq!(t.tls_ms, Some(45));
    assert_eq!(t.ttfb_ms, Some(120));
    // Non-applicable phases stay null.
    let t = check_timing(&check_result(None, None, None));
    assert_eq!(t.dns_ms, None);
    assert_eq!(t.ttfb_ms, None);
}

#[test]
fn incident_summary_maps_severity_and_latest_update() {
    let s = incident_summary(&active_incident(Some(update(
        IncidentStatusPhase::Identified,
    ))));
    assert_eq!(s.monitor_name, "api");
    assert_eq!(s.severity, "critical");
    assert_eq!(s.latest_phase.as_deref(), Some("identified"));
    assert!(s.latest_update_at.is_some());
}

#[test]
fn incident_summary_reports_resolved_at_once_ended() {
    let mut brief = active_incident(None);
    assert!(incident_summary(&brief).resolved_at.is_none());
    brief.ended_at = Some(Utc::now());
    assert!(incident_summary(&brief).resolved_at.is_some());
}

#[test]
fn an_incident_cursor_round_trips_its_whole_query() {
    let page = IncidentPage {
        offset: 50,
        open_only: false,
        range: TimeRange {
            from: Utc::now() - Duration::try_days(90).unwrap(),
            to: Utc::now(),
        },
        target_id: Some(Uuid::now_v7()),
    };
    let back: IncidentPage = cursor::decode_query(&cursor::encode_query(&page).unwrap()).unwrap();
    assert_eq!(back.offset, 50);
    assert!(!back.open_only);
    assert_eq!(back.target_id, page.target_id);
    assert_eq!(back.range.from, page.range.from);
    assert_eq!(back.range.to, page.range.to);
    assert!(cursor::decode_query::<IncidentPage>("not-a-cursor").is_none());
}

#[test]
fn a_probe_refusal_is_not_retryable_but_a_missing_agent_is() {
    let refused = probe_dispatch_error(crate::error::AppError::bad_request(
        "heartbeat_not_probeable",
        "nothing to probe",
    ));
    assert_eq!(refused.code, codes::INVALID_ARGUMENT);
    assert!(!refused.retryable);
    assert_eq!(outcome_for(&refused), Outcome::Denied);

    let unavailable = probe_dispatch_error(crate::error::AppError::service_unavailable(
        "no_agent",
        "no live agent",
    ));
    assert_eq!(unavailable.code, codes::PROBE_UNAVAILABLE);
    assert!(unavailable.retryable);
    assert_eq!(outcome_for(&unavailable), Outcome::Error);
}

#[test]
fn incident_state_filter_defaults_to_open() {
    assert!(parse_incident_state_filter(None).unwrap());
    assert!(parse_incident_state_filter(Some("open")).unwrap());
    assert!(!parse_incident_state_filter(Some("all")).unwrap());
    let err = parse_incident_state_filter(Some("resolved")).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
}

#[test]
fn incident_window_defaults_to_trailing_month() {
    let now = Utc::now();
    let r = incident_window(None, None, now).unwrap();
    assert_eq!(r.to, now);
    assert_eq!((r.to - r.from).num_days(), DEFAULT_INCIDENT_WINDOW_DAYS);
}

#[test]
fn incident_window_clamps_an_over_wide_span() {
    let now = Utc::now();
    let from = (now - Duration::try_days(3_000).unwrap()).to_rfc3339();
    let r = incident_window(Some(&from), None, now).unwrap();
    assert_eq!((r.to - r.from).num_days(), MAX_INCIDENT_WINDOW_DAYS);
}

#[test]
fn incident_window_rejects_bad_input() {
    let now = Utc::now();
    assert_eq!(
        incident_window(Some("yesterday"), None, now)
            .unwrap_err()
            .code,
        codes::INVALID_ARGUMENT
    );
    // `from` at or after `to` would silently return nothing.
    let from = now.to_rfc3339();
    let to = (now - Duration::try_hours(1).unwrap()).to_rfc3339();
    assert_eq!(
        incident_window(Some(&from), Some(&to), now)
            .unwrap_err()
            .code,
        codes::INVALID_ARGUMENT
    );
}

#[test]
fn public_text_trims_blank_and_caps_length() {
    assert_eq!(
        clean_public_text(Some("   "), "public_title", 10).unwrap(),
        None
    );
    assert_eq!(
        clean_public_text(Some("  hi  "), "public_title", 10).unwrap(),
        Some("hi".to_string())
    );
    let err = clean_public_text(Some("abcdefghijk"), "public_title", 10).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
}

#[test]
fn write_tools_are_filtered_out_for_a_client_that_cannot_confirm() {
    let tools = McpServer::tool_router().list_all();
    let (read, write): (Vec<_>, Vec<_>) = tools.iter().partition(|t| is_read_only(t));
    assert!(!write.is_empty());
    assert!(write.iter().any(|t| t.name == "publish_incident"));
    assert!(read.iter().any(|t| t.name == "list_incidents"));
    assert!(read.iter().any(|t| t.name == "list_regions"));
    assert!(read.iter().any(|t| t.name == "list_tags"));
    assert!(!read.iter().any(|t| t.name == "pause_monitor"));
}

/// The connector directory rejects a server whose tools lack either. The
/// title belongs on the tool, not in its annotations: clients read the
/// annotation only as a fallback, and untrusted-server guidance tells them
/// not to make decisions from annotations at all.
#[test]
fn every_tool_carries_a_title_and_the_hint_that_applies_to_it() {
    for tool in McpServer::tool_router().list_all() {
        assert!(
            tool.title.as_ref().is_some_and(|t| !t.trim().is_empty()),
            "{} has no title",
            tool.name
        );
        let ann = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
        assert!(
            ann.read_only_hint == Some(true) || ann.destructive_hint.is_some(),
            "{} is a write tool with no destructive hint, so it inherits the \
             spec default of destructive",
            tool.name
        );
    }
}

/// The landing page names every tool and counts them, and an assistant
/// quoting it cannot tell that the list went stale. Renaming or adding a
/// tool has to fail here rather than in someone's chat window.
#[test]
fn the_landing_page_names_every_tool_this_server_serves() {
    let page = crate::marketing::landings::LANDINGS
        .iter()
        .find(|l| l.path == "/mcp-server")
        .expect("the MCP landing exists");
    let prose: String = page.sections.iter().map(|s| s.body).collect();
    let tools = McpServer::tool_router().list_all();

    for tool in &tools {
        assert!(
            prose.contains(tool.name.as_ref()),
            "{} is served but the page never names it",
            tool.name
        );
    }

    let reads = tools
        .iter()
        .filter(|t| {
            t.annotations
                .as_ref()
                .is_some_and(|a| a.read_only_hint == Some(true))
        })
        .count();
    let claimed = page
        .features
        .iter()
        .find(|f| f.label == "Tools")
        .expect("the page states a tool count");
    assert_eq!(
        claimed.value,
        format!(
            "{} ({reads} read + {} fenced writes)",
            tools.len(),
            tools.len() - reads
        )
    );
}

fn http_check() -> CheckSpec {
    use crate::domain::{HttpCheck, HttpMethod};
    CheckSpec::Http(HttpCheck {
        url: "https://api.example.com/health".parse().unwrap(),
        method: HttpMethod::Head,
        timeout: std::time::Duration::from_secs(5),
        follow_redirects: false,
        max_redirects: 0,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: Some("ok".into()),
        headers: HashMap::from([("X-Api-Key".to_string(), "shh".to_string())]),
        body: Some("ping".into()),
        verify_tls: true,
        basic_auth: Some(("u".into(), "p".into())),
        bearer_token: Some("t0ken".into()),
    })
}

#[test]
fn an_http_config_reports_what_the_check_asserts() {
    let CheckConfig::Http(http) = check_config(&http_check()) else {
        panic!("expected http");
    };
    assert_eq!(http.method, "HEAD");
    assert_eq!(http.expected_status, "200");
    assert_eq!(http.timeout_ms, 5_000);
    assert!(!http.follow_redirects);
    assert_eq!(http.expected_body_contains.as_deref(), Some("ok"));
}

#[test]
fn a_header_is_reported_by_name_with_its_value_masked() {
    let config = check_config(&http_check());
    let CheckConfig::Http(http) = &config else {
        panic!("expected http");
    };
    // Which headers are sent is the diagnostic; the values are credentials.
    assert_eq!(
        http.headers.get("X-Api-Key").map(String::as_str),
        Some(REDACTED)
    );
    assert_eq!(http.body.as_deref(), Some(REDACTED));
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("shh"), "header value leaked: {json}");
    assert!(!json.contains("ping"), "request body leaked: {json}");
    let CheckSpec::Http(mut plain) = http_check() else {
        unreachable!()
    };
    plain.body = None;
    let CheckConfig::Http(plain) = check_config(&CheckSpec::Http(plain)) else {
        unreachable!()
    };
    assert_eq!(plain.body, None);
}

#[test]
fn credentials_are_reported_as_set_never_as_values() {
    let config = check_config(&http_check());
    let CheckConfig::Http(http) = &config else {
        panic!("expected http");
    };
    assert!(http.has_basic_auth);
    assert!(http.has_bearer_token);
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("t0ken"), "bearer token leaked: {json}");
    assert!(!json.contains("\"p\""), "basic auth leaked: {json}");
}

#[test]
fn expected_status_reads_as_one_phrase() {
    assert_eq!(expected_status_str(&ExpectedStatus::Exact(204)), "204");
    assert_eq!(
        expected_status_str(&ExpectedStatus::Range { min: 200, max: 299 }),
        "200-299"
    );
    assert_eq!(
        expected_status_str(&ExpectedStatus::OneOf(vec![200, 201, 204])),
        "200, 201, 204"
    );
}

#[test]
fn a_flow_config_numbers_its_steps_and_withholds_fill_values() {
    use crate::domain::FlowCheck;
    let config = check_config(&CheckSpec::Flow(FlowCheck {
        start_url: "https://app.example.com/login".parse().unwrap(),
        steps: vec![
            FlowStep::Fill {
                selector: "#password".into(),
                value: "hunter2".into(),
            },
            FlowStep::Click {
                selector: "#submit".into(),
            },
            FlowStep::AssertText {
                selector: None,
                contains: "Welcome".into(),
            },
        ],
        timeout: std::time::Duration::from_secs(30),
        step_timeout: std::time::Duration::from_secs(5),
        verify_tls: true,
    }));
    let CheckConfig::Flow(flow) = &config else {
        panic!("expected flow");
    };
    assert_eq!(
        flow.steps.iter().map(|s| s.step).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(flow.steps[0].op, "fill");
    assert!(flow.steps[0].value_withheld);
    assert_eq!(flow.steps[0].selector.as_deref(), Some("#password"));
    assert!(!flow.steps[1].value_withheld);
    assert_eq!(flow.steps[2].contains.as_deref(), Some("Welcome"));
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("hunter2"), "fill value leaked: {json}");
}

#[test]
fn a_heartbeat_config_carries_its_cadence_and_no_token() {
    use crate::domain::HeartbeatCheck;
    let config = check_config(&CheckSpec::Heartbeat(HeartbeatCheck {
        period: std::time::Duration::from_secs(300),
        grace: std::time::Duration::from_secs(60),
        max_runtime: None,
    }));
    let CheckConfig::Heartbeat(hb) = &config else {
        panic!("expected heartbeat");
    };
    assert_eq!((hb.period_secs, hb.grace_secs), (300, 60));
    assert_eq!(hb.max_runtime_secs, None);
    // The ping URL and token are the credential; the kind name is enough.
    assert!(!serde_json::to_string(&config).unwrap().contains("token"));
}

fn test_channel(id: Uuid, name: &str) -> NotificationChannel {
    use crate::domain::notification_channel::{ChannelConfig, ChannelKind, SlackConfig};
    NotificationChannel {
        id,
        name: name.to_string(),
        kind: ChannelKind::Slack,
        config: ChannelConfig::Slack(SlackConfig {
            webhook_url: "https://hooks.slack.example/T/B/x".into(),
            mention: None,
        }),
        enabled: true,
        disabled_reason: None,
        verified_at: None,
        consecutive_failures: 0,
        failing_since: None,
        last_delivered_at: None,
        write_source: WriteSource::Ui,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        auto_bind_tags: Vec::new(),
    }
}

fn stored_monitor() -> Target {
    Target {
        id: Uuid::nil(),
        name: "checkout".into(),
        check: http_check(),
        interval: std::time::Duration::from_secs(60),
        enabled: true,
        tags: vec!["prod".into(), "payments".into()],
        alerts: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        region_policy: RegionIncidentPolicy::Majority,
        group_name: Some("API".into()),
        owner_user_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        write_source: WriteSource::Ui,
        plan_hold_at: None,
    }
}

fn patch_args() -> UpdateMonitorArgs {
    UpdateMonitorArgs {
        id: Uuid::nil().to_string(),
        interval_secs: None,
        alert_confirmations: None,
        notify_recovery: None,
        renotify_interval_secs: None,
        tags: None,
        group_name: None,
        region_policy: None,
        channel_ids: None,
    }
}

#[test]
fn a_patch_reports_every_field_it_moves() {
    let args = UpdateMonitorArgs {
        interval_secs: Some(300),
        alert_confirmations: Some(3),
        region_policy: Some(RegionPolicyArg {
            mode: RegionPolicyMode::Count,
            count: Some(2),
        }),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap();
    assert_eq!(update.interval, Some(std::time::Duration::from_secs(300)));
    assert_eq!(update.alert_confirmations, Some(3));
    assert_eq!(update.region_policy, Some(RegionIncidentPolicy::Count(2)));
    let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(
        fields,
        vec!["interval_secs", "alert_confirmations", "region_policy"]
    );
    let interval = &changes[0];
    assert_eq!(
        (interval.from.as_str(), interval.to.as_str()),
        ("60", "300")
    );
}

/// The write path builds the patch twice, once for the prompt and once
/// after approval, and refuses as `conflict` if they differ. A field diffed
/// outside this function makes that guard fire on an unchanged monitor.
#[test]
fn the_patch_is_the_same_both_times_it_is_built() {
    let channel = Uuid::now_v7();
    let channels = vec![test_channel(channel, "ops-slack")];
    let args = UpdateMonitorArgs {
        interval_secs: Some(300),
        tags: Some(vec!["prod".into()]),
        channel_ids: Some(vec![channel.to_string()]),
        ..patch_args()
    };
    let target = stored_monitor();
    let (_, first) = build_monitor_patch(&args, &target, &channels, 3).unwrap();
    let (update, second) = build_monitor_patch(&args, &target, &channels, 3).unwrap();
    assert_eq!(first, second);
    assert!(
        first.iter().any(|c| c.field == "alerts"),
        "channels must be part of the diff, not beside it: {first:?}"
    );
    assert_eq!(
        update.alerts,
        Some(TargetAlerts(vec![AlertBinding {
            channel_id: channel
        }]))
    );
}

#[test]
fn a_value_that_already_matches_is_not_a_change() {
    let args = UpdateMonitorArgs {
        interval_secs: Some(60),
        notify_recovery: Some(true),
        tags: Some(vec!["payments".into(), "prod".into()]),
        region_policy: Some(RegionPolicyArg {
            mode: RegionPolicyMode::Majority,
            count: None,
        }),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    assert!(update.interval.is_none());
    assert!(update.tags.is_none());
}

#[test]
fn a_dropped_tag_is_spelled_out_before_it_happens() {
    let args = UpdateMonitorArgs {
        tags: Some(vec!["prod".into(), "prod".into()]),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap();
    assert_eq!(update.tags, Some(vec!["prod".to_string()]));
    assert_eq!(changes[0].from, "prod, payments");
    assert_eq!(changes[0].to, "prod");

    // A blank is refused rather than quietly dropped, which would have read
    // back as a list the caller never sent.
    let blank = UpdateMonitorArgs {
        tags: Some(vec!["prod".into(), "  ".into()]),
        ..patch_args()
    };
    let err = build_monitor_patch(&blank, &stored_monitor(), &[], 3).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
}

#[test]
fn a_group_clears_on_null_and_refuses_a_blank() {
    let cleared = UpdateMonitorArgs {
        group_name: Some(None),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&cleared, &stored_monitor(), &[], 3).unwrap();
    assert_eq!(update.group_name, Some(None));
    assert_eq!(
        (changes[0].from.as_str(), changes[0].to.as_str()),
        ("API", "none")
    );

    let blank = UpdateMonitorArgs {
        group_name: Some(Some("   ".into())),
        ..patch_args()
    };
    let err = build_monitor_patch(&blank, &stored_monitor(), &[], 3).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
}

#[test]
fn a_second_count_too_wide_for_the_column_is_refused_not_wrapped() {
    // 2^32 + 60 lands on 60 after the i32 cast.
    let args = UpdateMonitorArgs {
        interval_secs: Some(4_294_967_356),
        ..patch_args()
    };
    let err = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    let args = UpdateMonitorArgs {
        renotify_interval_secs: Some(u32::MAX),
        ..patch_args()
    };
    assert!(build_monitor_patch(&args, &stored_monitor(), &[], 3).is_err());
    let args = UpdateMonitorArgs {
        alert_confirmations: Some(u32::MAX),
        ..patch_args()
    };
    assert!(build_monitor_patch(&args, &stored_monitor(), &[], 3).is_err());
    let args = UpdateMonitorArgs {
        interval_secs: Some(86_400),
        ..patch_args()
    };
    assert!(build_monitor_patch(&args, &stored_monitor(), &[], 3).is_ok());
}

#[test]
fn a_mid_flow_nav_url_loses_what_could_be_a_token() {
    use crate::domain::FlowCheck;
    let config = check_config(&CheckSpec::Flow(FlowCheck {
        start_url: "https://app.example.com/login".parse().unwrap(),
        steps: vec![FlowStep::Goto {
            url: "https://u:p@app.example.com/enter?token=letmein#frag"
                .parse()
                .unwrap(),
        }],
        timeout: std::time::Duration::from_secs(30),
        step_timeout: std::time::Duration::from_secs(5),
        verify_tls: true,
    }));
    let CheckConfig::Flow(flow) = &config else {
        panic!("expected flow");
    };
    assert_eq!(
        flow.steps[0].url.as_deref(),
        Some("https://app.example.com/enter")
    );
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("letmein"), "nav token leaked: {json}");
}

#[test]
fn an_audit_row_can_tell_a_real_change_from_a_no_op() {
    let applied = MonitorUpdateResult {
        id: Uuid::nil().to_string(),
        changes: vec![FieldChange {
            field: "interval_secs".into(),
            from: "60".into(),
            to: "300".into(),
        }],
    };
    let recorded = json!({ "id": applied.id, "changes": applied.changes });
    assert_eq!(recorded["changes"][0]["field"], "interval_secs");
    assert_eq!(recorded["changes"][0]["to"], "300");

    let no_op = json!({ "id": Uuid::nil().to_string(), "changes": Vec::<FieldChange>::new() });
    assert_eq!(no_op["changes"].as_array().unwrap().len(), 0);

    let args = UpdateMonitorArgs {
        interval_secs: Some(300),
        tags: Some(vec!["prod".into()]),
        ..patch_args()
    };
    assert_eq!(requested_fields(&args), vec!["interval_secs", "tags"]);
}

fn http_with_body(body: &str) -> NewCheck {
    let mut check = http_with_headers(&[]);
    if let NewCheck::Http { body: b, .. } = &mut check {
        *b = Some(body.to_string());
    }
    check
}

fn http_with_headers(headers: &[(&str, &str)]) -> NewCheck {
    NewCheck::Http {
        url: "https://api.example.com/v1/chat".into(),
        method: Some("post".into()),
        expected_status: None,
        expected_body_contains: None,
        timeout_ms: None,
        follow_redirects: None,
        verify_tls: None,
        headers: Some(
            headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        ),
        body: None,
    }
}

#[test]
fn a_credential_in_the_body_must_reference_a_variable_too() {
    for body in [
        "grant_type=client_credentials&client_secret=sk-live-abc",
        r#"{"password": "hunter2"}"#,
        "api_key=abc123",
    ] {
        let check = http_with_body(body);
        let err = new_check_spec(&check).expect_err(body);
        assert_eq!(err.code, codes::INVALID_ARGUMENT, "{body}");
        assert!(err.message.contains("list_variables"), "{}", err.message);
    }
}

#[test]
fn a_body_referencing_a_variable_is_accepted() {
    for body in [
        "grant_type=client_credentials&client_secret={{ oauth_secret }}",
        r#"{"password": "{{ login_password }}"}"#,
        "note=nothing sensitive here",
    ] {
        assert!(new_check_spec(&http_with_body(body)).is_ok(), "{body}");
    }
}

#[test]
fn credential_headers_are_redacted_downstream() {
    for name in CREDENTIAL_HEADERS {
        assert!(
            crate::worker::http_check::PROBE_REDACT_HEADERS.contains(name),
            "`{name}` is gated here but would still be previewed and sent across a redirect"
        );
    }
}

#[test]
fn a_credential_header_must_reference_a_variable_not_carry_one() {
    for value in [
        "Bearer sk-live-abc123",
        "sk-live-abc123",
        "Bearer sk-live {{ api_key }}",
        "{{ api_key }} trailing",
        "{{ api_key }}{{ other }}",
        "Bearer {{ Api_Key }}",
    ] {
        let err = new_check_spec(&http_with_headers(&[("Authorization", value)]))
            .expect_err("a pasted credential is refused");
        assert_eq!(err.code, crate::mcp::error::codes::INVALID_ARGUMENT);
        assert!(
            err.message.contains("list_variables"),
            "the refusal points at the supported path, got: {}",
            err.message
        );
    }
}

#[test]
fn a_credential_header_referencing_a_variable_is_accepted() {
    for value in [
        "Bearer {{ api_key }}",
        "{{ session_cookie }}",
        "Basic {{ b64 }}",
    ] {
        let spec = new_check_spec(&http_with_headers(&[("Authorization", value)]))
            .expect("a reference is the supported shape");
        let CheckSpec::Http(http) = spec else {
            panic!("expected http");
        };
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some(value)
        );
    }
}

#[test]
fn a_plain_header_still_takes_literal_text() {
    let spec = new_check_spec(&http_with_headers(&[("Content-Type", "application/json")]))
        .expect("a non-credential header carries literal text");
    let CheckSpec::Http(http) = spec else {
        panic!("expected http");
    };
    assert_eq!(
        http.headers.get("Content-Type").map(String::as_str),
        Some("application/json")
    );
}

#[test]
fn every_credential_header_name_is_covered_case_insensitively() {
    for name in [
        "authorization",
        "Proxy-Authorization",
        "X-API-Key",
        "api-key",
        "Cookie",
    ] {
        let err = new_check_spec(&http_with_headers(&[(name, "hunter2")]))
            .expect_err("a credential header name is matched case-insensitively");
        assert_eq!(
            err.code,
            crate::mcp::error::codes::INVALID_ARGUMENT,
            "{name} should be treated as credential-bearing"
        );
    }
}

#[test]
fn a_malformed_header_name_is_refused() {
    let err = new_check_spec(&http_with_headers(&[("bad header", "x")]))
        .expect_err("a space is not a header name");
    assert!(err.message.contains("not a valid header name"));
}

#[test]
fn a_new_check_carries_no_credential_slot() {
    let CheckSpec::Http(http) = new_check_spec(&NewCheck::Http {
        url: "https://api.example.com/health".into(),
        method: Some("post".into()),
        expected_status: Some("200,204".into()),
        expected_body_contains: Some("ok".into()),
        timeout_ms: Some(2_000),
        follow_redirects: Some(false),
        verify_tls: None,
        headers: None,
        body: None,
    })
    .unwrap() else {
        panic!("expected http");
    };
    assert!(http.headers.is_empty());
    assert_eq!(http.body, None);
    assert!(http.basic_auth.is_none() && http.bearer_token.is_none());
    assert!(http.verify_tls, "a check defaults to verifying TLS");
    // Not following redirects means not budgeting for any.
    assert_eq!(http.max_redirects, 0);
    assert_eq!(expected_status_str(&http.expected_status), "200, 204");
}

#[test]
fn an_expected_status_takes_a_code_a_range_or_a_list() {
    let parsed = |s: Option<&str>| expected_status_str(&parse_expected_status(s).unwrap());
    assert_eq!(parsed(None), "200-299");
    assert_eq!(parsed(Some("204")), "204");
    assert_eq!(parsed(Some("200-299")), "200-299");
    assert_eq!(parsed(Some(" 200 , 201 ")), "200, 201");
    // A backwards range can never match, so it is refused rather than stored.
    assert!(parse_expected_status(Some("299-200")).is_err());
    assert!(parse_expected_status(Some("2xx")).is_err());
    // Round-trips against the renderer the read tools use.
    for spec in ["204", "200-299"] {
        assert_eq!(
            expected_status_str(&parse_expected_status(Some(spec)).unwrap()),
            spec
        );
    }
}

#[test]
fn a_creation_prompt_states_every_setting_it_would_apply() {
    let mut new = NewTarget {
        name: "checkout".into(),
        check: http_check(),
        interval: std::time::Duration::from_secs(300),
        enabled: true,
        tags: vec!["prod".into()],
        alerts: Default::default(),
        region_policy: Some(RegionIncidentPolicy::Count(2)),
        alert_confirmations: 5,
        notify_recovery: false,
        renotify_interval_secs: 0,
        group_name: Some("API".into()),
        owner_user_id: None,
        regions: None,
    };
    new.alerts = TargetAlerts(vec![AlertBinding {
        channel_id: Uuid::nil(),
    }]);
    let summary = "ops-slack, pager (disabled, delivers nothing)";
    let regions = vec!["eu-helsinki".to_string(), "us-east".to_string()];
    let lines = create_prompt_lines(&new, &regions, None, Some(summary)).join("\n");
    for expected in [
        "checked every 300s",
        "probed from: eu-helsinki, us-east",
        "tags: prod",
        "group: API",
        "alerts after 5 failing checks",
        "recovery is not announced",
        "no reminders",
        "2 regions down (2 of 2 assigned regions)",
        "notification channels: ops-slack, pager (disabled, delivers nothing)",
    ] {
        assert!(
            lines.contains(expected),
            "{expected} missing from:\n{lines}"
        );
    }

    // Defaults are still stated: silence would read as "unset", and the
    // operator is approving them either way.
    new.notify_recovery = true;
    new.renotify_interval_secs = 3_600;
    new.tags.clear();
    new.group_name = None;
    new.alerts = TargetAlerts::default();
    let lines = create_prompt_lines(&new, &regions, None, None).join("\n");
    assert!(lines.contains("first reminder after 3600s"));
    assert!(!lines.contains("tags:"));
    // Silence is the one state worth stating outright.
    assert!(lines.contains("alerts nobody unless a channel's tag rule covers its tags"));
}

#[test]
fn a_url_carrying_a_password_is_refused_rather_than_stored() {
    let err = new_check_spec(&NewCheck::Http {
        url: "https://admin:hunter2@api.example.com/health".into(),
        method: None,
        expected_status: None,
        expected_body_contains: None,
        timeout_ms: None,
        follow_redirects: None,
        verify_tls: None,
        headers: None,
        body: None,
    })
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    assert!(!err.message.contains("hunter2"), "{}", err.message);
}

/// The read side of the kinds the write side can now create: what each one
/// asserts has to reach the model, or it reasons from a partial picture.
#[test]
fn the_non_http_kinds_report_what_they_assert() {
    let cfg = |json: &str| check_config(&serde_json::from_str::<CheckSpec>(json).unwrap());

    let CheckConfig::Tcp(tcp) =
        cfg(r#"{"type":"tcp","host":"db.internal","port":5432,"timeout":3000}"#)
    else {
        panic!("expected tcp");
    };
    assert_eq!((tcp.host.as_str(), tcp.port), ("db.internal", 5432));
    assert_eq!(tcp.timeout_ms, 3_000);

    let CheckConfig::Dns(dns) = cfg(
        r#"{"type":"dns","domain":"api.acme.com","record_type":"CNAME","expected_contains":"edge","timeout":2000}"#,
    ) else {
        panic!("expected dns");
    };
    // Reads report the wire spelling; `create_monitor` accepts either case,
    // so a value round-tripped from `get_monitor` still parses.
    assert_eq!(dns.record_type, "CNAME");
    assert_eq!(dns.expected_contains.as_deref(), Some("edge"));

    let CheckConfig::TlsCert(tls) = cfg(
        r#"{"type":"tls_cert","host":"acme.com","port":8443,"warn_days":21,"critical_days":3,"timeout":5000}"#,
    ) else {
        panic!("expected tls_cert");
    };
    assert_eq!((tls.port, tls.warn_days, tls.critical_days), (8443, 21, 3));

    let CheckConfig::DomainExpiry(reg) = cfg(
        r#"{"type":"domain_expiry","domain":"acme.com","warn_days":60,"critical_days":14,"timeout":5000}"#,
    ) else {
        panic!("expected domain_expiry");
    };
    assert_eq!((reg.warn_days, reg.critical_days), (60, 14));
    assert_eq!(reg.domain, "acme.com");
}

#[test]
fn every_creatable_kind_converts_with_the_defaults_it_documents() {
    let spec = |c: NewCheck| new_check_spec(&c).unwrap();

    let CheckSpec::Http(http) = spec(NewCheck::Http {
        url: "https://example.com/health".into(),
        method: None,
        expected_status: None,
        expected_body_contains: None,
        timeout_ms: None,
        follow_redirects: None,
        verify_tls: None,
        headers: None,
        body: None,
    }) else {
        panic!("expected http");
    };
    assert_eq!(http.method, crate::domain::HttpMethod::Get);
    assert_eq!(http.timeout, std::time::Duration::from_secs(10));
    assert!(http.follow_redirects);
    // Following redirects means budgeting for some.
    assert_eq!(http.max_redirects, 5);
    assert_eq!(expected_status_str(&http.expected_status), "200-299");

    let CheckSpec::Heartbeat(hb) = spec(NewCheck::Heartbeat {
        period_secs: 3_600,
        grace_secs: 300,
        max_runtime_secs: Some(120),
    }) else {
        panic!("expected heartbeat");
    };
    assert_eq!(hb.period, std::time::Duration::from_secs(3_600));
    assert_eq!(hb.grace, std::time::Duration::from_secs(300));
    assert_eq!(hb.max_runtime, Some(std::time::Duration::from_secs(120)));

    let CheckSpec::Tcp(tcp) = spec(NewCheck::Tcp {
        host: "db.example.com".into(),
        port: 5432,
        timeout_ms: None,
    }) else {
        panic!("expected tcp");
    };
    assert_eq!((tcp.host.as_str(), tcp.port), ("db.example.com", 5432));
    assert_eq!(tcp.timeout, std::time::Duration::from_secs(5));

    let CheckSpec::Ping(ping) = spec(NewCheck::Ping {
        host: "example.com".into(),
        timeout_ms: Some(1_500),
    }) else {
        panic!("expected ping");
    };
    assert_eq!(ping.timeout, std::time::Duration::from_millis(1_500));

    let CheckSpec::Dns(dns) = spec(NewCheck::Dns {
        domain: "example.com".into(),
        record_type: None,
        resolver: None,
        expected_contains: None,
        timeout_ms: None,
    }) else {
        panic!("expected dns");
    };
    assert_eq!(dns.record_type, crate::domain::DnsRecordType::A);
    assert!(dns.resolver.is_none(), "defaults to the agent's own");

    let CheckSpec::TlsCert(tls) = spec(NewCheck::TlsCert {
        host: "example.com".into(),
        port: None,
        warn_days: None,
        critical_days: None,
        timeout_ms: None,
    }) else {
        panic!("expected tls_cert");
    };
    assert_eq!((tls.port, tls.warn_days, tls.critical_days), (443, 30, 7));
    assert!(tls.server_name.is_none());

    let CheckSpec::DomainExpiry(reg) = spec(NewCheck::DomainExpiry {
        domain: "example.com".into(),
        warn_days: Some(45),
        critical_days: None,
        timeout_ms: None,
    }) else {
        panic!("expected domain_expiry");
    };
    assert_eq!((reg.warn_days, reg.critical_days), (45, 7));
}

#[test]
fn an_unknown_method_or_record_type_names_the_choices() {
    let err = new_check_spec(&NewCheck::Http {
        url: "https://example.com/".into(),
        method: Some("fetch".into()),
        expected_status: None,
        expected_body_contains: None,
        timeout_ms: None,
        follow_redirects: None,
        verify_tls: None,
        headers: None,
        body: None,
    })
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    assert!(err.message.contains("get"), "{}", err.message);

    let err = new_check_spec(&NewCheck::Dns {
        domain: "example.com".into(),
        record_type: Some("alias".into()),
        resolver: None,
        expected_contains: None,
        timeout_ms: None,
    })
    .unwrap_err();
    assert!(err.message.contains("cname"), "{}", err.message);
    // Case is not the caller's problem.
    assert!(
        new_check_spec(&NewCheck::Dns {
            domain: "example.com".into(),
            record_type: Some("CNAME".into()),
            resolver: None,
            expected_contains: None,
            timeout_ms: None,
        })
        .is_ok()
    );
}

#[test]
fn an_omitted_interval_opens_where_the_picker_does() {
    let http = http_check();
    // The plan floor wins when it is stricter than the kind's opening.
    assert_eq!(default_interval_secs(&http, 300), 300);
    let tls: CheckSpec = serde_json::from_str(
        r#"{"type":"tls_cert","host":"a.com","port":443,"warn_days":30,"critical_days":7,"timeout":5000}"#,
    )
    .unwrap();
    assert_eq!(
        default_interval_secs(&tls, 60),
        43_200,
        "the picker's opening, not the 3600s hard minimum, which would probe \
         a certificate twelve times harder"
    );

    // A heartbeat cannot tick slower than the window it judges, even when
    // the kind's opening is coarser than the window the caller gave.
    let hb = new_check_spec(&NewCheck::Heartbeat {
        period_secs: 60,
        grace_secs: 30,
        max_runtime_secs: None,
    })
    .unwrap();
    assert_eq!(default_interval_secs(&hb, 10), 90);
    // ...but never under the plan floor, which would be refused as an
    // interval the caller never sent.
    assert_eq!(default_interval_secs(&hb, 180), 180);
}

#[test]
fn a_binding_is_a_set_of_channels_the_org_owns() {
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let channels = vec![test_channel(a, "ops"), test_channel(b, "pager")];

    let bound = resolve_bindings(&[a.to_string(), b.to_string()], &channels).unwrap();
    assert_eq!(bound.iter().count(), 2);
    assert!(resolve_bindings(&[], &channels).unwrap().is_empty());

    let foreign = Uuid::now_v7();
    let err = resolve_bindings(&[foreign.to_string()], &channels).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    assert!(err.message.contains("this organization"), "{}", err.message);

    let err = resolve_bindings(&[a.to_string(), a.to_string()], &channels).unwrap_err();
    assert!(err.message.contains("twice"), "{}", err.message);

    assert_eq!(
        resolve_bindings(&["not-a-uuid".into()], &channels)
            .unwrap_err()
            .code,
        codes::INVALID_ARGUMENT
    );
}

#[test]
fn reordering_the_same_channels_is_not_a_change() {
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let channels = vec![test_channel(a, "ops"), test_channel(b, "pager")];
    let mut target = stored_monitor();
    target.alerts = TargetAlerts(vec![
        AlertBinding { channel_id: a },
        AlertBinding { channel_id: b },
    ]);

    let args = UpdateMonitorArgs {
        channel_ids: Some(vec![b.to_string(), a.to_string()]),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &target, &channels, 3).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    assert!(update.alerts.is_none());

    // Dropping one is a change, and the prompt shows what leaves.
    let args = UpdateMonitorArgs {
        channel_ids: Some(vec![a.to_string()]),
        ..patch_args()
    };
    let (_, changes) = build_monitor_patch(&args, &target, &channels, 3).unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].from, "ops, pager");
    assert_eq!(changes[0].to, "ops");

    // Clearing every binding is the destructive case the prompt exists for.
    let args = UpdateMonitorArgs {
        channel_ids: Some(vec![]),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &target, &channels, 3).unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        (changes[0].from.as_str(), changes[0].to.as_str()),
        ("ops, pager", "nobody")
    );
    assert_eq!(update.alerts, Some(TargetAlerts::default()));
}

#[test]
fn a_channel_that_delivers_nothing_says_so_where_it_is_named() {
    let (live, off, unverified, dead, gone) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let mut disabled = test_channel(off, "pager");
    disabled.enabled = false;
    // Kind and config have to agree: a Slack config on an Email channel is
    // a row the store cannot hold, and the next rule that reads config
    // would be tested against a fiction.
    let mut email = test_channel(unverified, "on-call inbox");
    email.kind = crate::domain::notification_channel::ChannelKind::Email;
    email.config = crate::domain::notification_channel::ChannelConfig::Email(
        crate::domain::notification_channel::EmailConfig {
            to: "on-call@example.com".into(),
        },
    );
    // Enabled and confirmed, so nothing but its delivery run marks it.
    let mut silent = test_channel(dead, "ops-webhook");
    silent.consecutive_failures = 3;
    silent.failing_since = Some(Utc::now());
    let channels = vec![test_channel(live, "ops"), disabled, email, silent];

    assert_eq!(undeliverable_reason(&channels[0], 3), None);
    assert_eq!(
        undeliverable_reason(&channels[1], 3),
        Some("disabled, delivers nothing")
    );
    assert_eq!(
        undeliverable_reason(&channels[2], 3),
        Some("address never verified, delivers nothing")
    );
    assert_eq!(
        undeliverable_reason(&channels[3], 3),
        Some("recent alerts did not arrive")
    );
    assert_eq!(
        undeliverable_reason(&channels[3], 0),
        None,
        "a limit of zero switches the flag off"
    );

    let named = |ids: &[Uuid]| {
        channel_names(
            &TargetAlerts(
                ids.iter()
                    .map(|id| AlertBinding { channel_id: *id })
                    .collect(),
            ),
            &[],
            &channels,
            3,
        )
    };
    assert_eq!(named(&[]), None);
    assert_eq!(named(&[live]).as_deref(), Some("ops"));
    assert_eq!(
        named(&[off]).as_deref(),
        Some("pager (disabled, delivers nothing)")
    );
    assert_eq!(
        named(&[unverified]).as_deref(),
        Some("on-call inbox (address never verified, delivers nothing)")
    );
    assert_eq!(
        named(&[dead]).as_deref(),
        Some("ops-webhook (recent alerts did not arrive)")
    );
    // A binding whose channel is gone is named, not dropped: a shorter list
    // would read as one fewer channel losing its alerts.
    assert_eq!(
        named(&[gone]).as_deref(),
        Some(format!("a deleted channel ({gone})").as_str())
    );
}

/// A retag alone can hand paging from one channel to another. A confirmation
/// that listed only the tags would have the human approve that unseen.
#[test]
fn a_retag_that_moves_coverage_says_so_without_any_binding_change() {
    let mut by_rule = test_channel(Uuid::now_v7(), "db team");
    by_rule.auto_bind_tags = vec!["payments".into()];
    let channels = vec![by_rule];

    let args = UpdateMonitorArgs {
        tags: Some(vec!["web".into()]),
        ..patch_args()
    };
    let (update, changes) = build_monitor_patch(&args, &stored_monitor(), &channels, 3).unwrap();
    let alerts = changes
        .iter()
        .find(|c| c.field == "alerts")
        .expect("a retag that drops the only covering channel is a change");
    assert_eq!(alerts.from, "db team (by tag)");
    assert_eq!(alerts.to, "nobody");
    // Nothing was rebound, so the patch must not write an alerts list.
    assert!(update.alerts.is_none());
}

/// Reading coverage off the tags being replaced shows the human a paging set
/// the patch does not produce, and they approve the wrong one.
#[test]
fn the_alerts_diff_reads_coverage_off_the_tags_the_same_call_sets() {
    let bound = Uuid::now_v7();
    let mut by_rule = test_channel(Uuid::now_v7(), "db team");
    by_rule.auto_bind_tags = vec!["payments".into()];
    let channels = vec![test_channel(bound, "ops-slack"), by_rule];

    // Stored tag `payments` is covered by the rule; `web` is not.
    let args = UpdateMonitorArgs {
        tags: Some(vec!["web".into()]),
        channel_ids: Some(vec![bound.to_string()]),
        ..patch_args()
    };
    let (_, changes) = build_monitor_patch(&args, &stored_monitor(), &channels, 3).unwrap();
    let alerts = changes.iter().find(|c| c.field == "alerts").unwrap();
    assert_eq!(alerts.from, "db team (by tag)");
    assert_eq!(
        alerts.to, "ops-slack",
        "the rule stops covering it, so the after side must not name it"
    );
}

/// A monitor covered only by a channel's tag rule is paged, so a confirmation
/// that called it unreachable would talk the operator out of a working setup.
#[test]
fn a_tag_rule_names_the_channel_it_covers_a_monitor_through() {
    let id = Uuid::now_v7();
    let mut by_rule = test_channel(id, "ops");
    by_rule.auto_bind_tags = vec!["db".into()];
    let channels = vec![by_rule];
    let rule_only = channel_names(&TargetAlerts::default(), &["db".to_string()], &channels, 3);
    assert_eq!(rule_only.as_deref(), Some("ops (by tag)"));
    // Bound and matching: named once, as the binding.
    let both = channel_names(
        &TargetAlerts(vec![AlertBinding { channel_id: id }]),
        &["db".to_string()],
        &channels,
        3,
    );
    assert_eq!(both.as_deref(), Some("ops"));
    // A rule nothing carries leaves the monitor unreachable.
    assert_eq!(
        channel_names(&TargetAlerts::default(), &["web".to_string()], &channels, 3),
        None
    );
}

#[test]
fn a_heartbeat_is_created_without_anything_to_probe() {
    let spec = new_check_spec(&NewCheck::Heartbeat {
        period_secs: 3_600,
        grace_secs: 300,
        max_runtime_secs: None,
    })
    .unwrap();
    assert!(spec.is_passive(), "a heartbeat must skip the trial run");
}

#[test]
fn a_trial_run_reads_as_one_line() {
    let outcome = |state: &str, http: Option<u16>, err: Option<&str>| ProbeOutcome {
        state: state.into(),
        duration_ms: 143,
        http_status: http,
        error: err.map(str::to_string),
        diagnostic: None,
    };
    assert_eq!(
        probe_line(&outcome("up", Some(200), None)),
        "passed, HTTP 200 in 143ms"
    );
    assert_eq!(
        probe_line(&outcome("down", Some(503), Some("upstream refused"))),
        "down, HTTP 503 in 143ms — upstream refused"
    );
    assert_eq!(
        probe_line(&outcome("error", None, Some("dns failure"))),
        "error in 143ms — dns failure"
    );
}

#[test]
fn a_quorum_reads_back_in_the_shape_it_is_written() {
    for stored in [
        RegionIncidentPolicy::Any,
        RegionIncidentPolicy::Majority,
        RegionIncidentPolicy::All,
        RegionIncidentPolicy::Count(3),
    ] {
        let view = region_policy_view(stored);
        let sent: RegionPolicyArg = serde_json::from_value(serde_json::to_value(&view).unwrap())
            .expect("the read shape deserializes as the write shape");
        assert_eq!(
            parse_region_policy(&sent).unwrap(),
            stored,
            "{view:?} did not round-trip"
        );
    }
}

/// `get_monitor` claims to carry every field `update_monitor` can change.
/// Without this, adding an editable field silently makes that claim false
/// and leaves the model reading from a partial picture.
#[test]
fn everything_writable_is_readable() {
    fn properties<T: schemars::JsonSchema>() -> std::collections::BTreeSet<String> {
        let schema = serde_json::to_value(schemars::schema_for!(T)).unwrap();
        schema["properties"]
            .as_object()
            .expect("an object schema")
            .keys()
            .cloned()
            .collect()
    }

    let readable = properties::<MonitorDetail>();
    for field in properties::<UpdateMonitorArgs>() {
        // The monitor being addressed, not a property of it.
        if field == "id" {
            continue;
        }
        // Ids are read back under a name that says what they are ids of.
        let expected = if field == "channel_ids" {
            "alert_channel_ids".to_string()
        } else {
            field
        };
        assert!(
            readable.contains(&expected),
            "update_monitor can change `{expected}`, but get_monitor does not report it"
        );
    }
}

#[test]
fn a_count_quorum_needs_a_count() {
    assert_eq!(
        parse_region_policy(&RegionPolicyArg {
            mode: RegionPolicyMode::Any,
            count: None
        })
        .unwrap(),
        RegionIncidentPolicy::Any
    );
    let err = parse_region_policy(&RegionPolicyArg {
        mode: RegionPolicyMode::Count,
        count: None,
    })
    .unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    // The schema carries the legal modes, so a typo never reaches the tool.
    assert!(
        serde_json::from_value::<RegionPolicyArg>(json!({"mode": "quorum", "count": 2})).is_err()
    );
}

#[test]
fn a_terraform_managed_monitor_is_refused_and_told_where_to_go() {
    let mut target = stored_monitor();
    assert!(deny_terraform(&target).is_ok());
    target.write_source = WriteSource::Terraform;
    let err = deny_terraform(&target).unwrap_err();
    assert_eq!(err.code, codes::MANAGED_EXTERNALLY);
    assert!(!err.retryable, "waiting does not make it editable");
    assert!(err.message.contains(".tf"), "{}", err.message);
    assert_eq!(outcome_for(&err), Outcome::Denied);
}

#[test]
fn an_update_that_names_an_uneditable_field_is_an_error_not_a_no_op() {
    let err = serde_json::from_value::<UpdateMonitorArgs>(json!({
        "id": Uuid::nil().to_string(),
        "address": "https://elsewhere.example.com",
    }))
    .unwrap_err();
    assert!(err.to_string().contains("address"), "{err}");
}

#[test]
fn a_region_filter_must_name_a_region_the_monitor_runs_in() {
    let assigned = vec!["eu-helsinki".to_string(), "apac-sg".to_string()];
    assert_eq!(requested_region(None, &assigned).unwrap(), None);
    assert_eq!(requested_region(Some("  "), &assigned).unwrap(), None);
    assert_eq!(
        requested_region(Some("apac-sg"), &assigned)
            .unwrap()
            .as_deref(),
        Some("apac-sg")
    );
    let err = requested_region(Some("us-east"), &assigned).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);
    assert!(err.message.contains("eu-helsinki"), "{}", err.message);
    assert!(requested_region(Some("eu-helsinki"), &[]).is_err());
}

#[test]
fn region_health_rates_a_regions_own_checks() {
    let health = region_health(crate::api::types::RegionRollup {
        region: "apac-sg".into(),
        samples: 200,
        up: 190,
        p50_ms: 120,
        p95_ms: 300,
        p99_ms: 900,
        last_status: "down".into(),
    });
    assert_eq!(health.uptime_pct, Some(95.0));
    assert_eq!(health.last_status, "down");
    let health = region_health(crate::api::types::RegionRollup {
        region: "us-east".into(),
        samples: 0,
        up: 0,
        p50_ms: 0,
        p95_ms: 0,
        p99_ms: 0,
        last_status: "weird".into(),
    });
    assert_eq!(health.uptime_pct, None);
    assert_eq!(health.last_status, "no_data");
}

fn region_option(id: &str) -> crate::storage::RegionOption {
    crate::storage::RegionOption {
        id: id.into(),
        name: id.into(),
        city: String::new(),
        country_code: None,
        continent: None,
        latitude: None,
        longitude: None,
    }
}

#[test]
fn the_region_catalog_flags_the_default_set_the_plan_actually_pays_for() {
    use crate::api::handlers::targets::default_region_set;

    let catalog = || {
        vec![
            region_option("apac-sg"),
            region_option("eu-frankfurt"),
            region_option("eu-helsinki"),
            region_option("us-east"),
        ]
    };
    let preferred = vec![
        "apac-sg".to_string(),
        "eu-frankfurt".to_string(),
        "eu-helsinki".to_string(),
    ];

    let applied = default_region_set(preferred.clone(), 3, "eu-helsinki");
    let flagged: Vec<String> = region_items(catalog(), &applied)
        .into_iter()
        .filter(|r| r.default_selected)
        .map(|r| r.id)
        .collect();
    assert_eq!(flagged, ["apac-sg", "eu-frankfurt", "eu-helsinki"]);

    let applied = default_region_set(preferred, 2, "eu-helsinki");
    let flagged: Vec<String> = region_items(catalog(), &applied)
        .into_iter()
        .filter(|r| r.default_selected)
        .map(|r| r.id)
        .collect();
    assert_eq!(flagged, ["apac-sg", "eu-helsinki"]);

    let applied = default_region_set(Vec::new(), 3, "eu-helsinki");
    let flagged: Vec<String> = region_items(catalog(), &applied)
        .into_iter()
        .filter(|r| r.default_selected)
        .map(|r| r.id)
        .collect();
    assert_eq!(flagged, ["eu-helsinki"]);
}

#[test]
fn the_region_cap_is_reported_only_where_it_bites() {
    assert_eq!(
        region_cap(3, 5),
        Some(3),
        "a plan short of the catalog says so"
    );
    assert_eq!(
        region_cap(i32::MAX, 5),
        None,
        "a cap the catalog can never reach is not a ceiling worth naming"
    );
    assert_eq!(
        region_cap(5, 5),
        None,
        "reaching every region is not licence to name every region"
    );
    assert_eq!(region_cap(1, 5), Some(1), "a single-region plan says one");
}

#[test]
fn incident_summary_no_update_yields_null_phase() {
    let s = incident_summary(&active_incident(None));
    assert!(s.latest_phase.is_none());
    assert!(s.latest_update_at.is_none());
}

#[test]
fn incident_detail_maps_state_severity_and_updates() {
    let inc = Incident {
        id: Uuid::nil(),
        target_id: Some(Uuid::nil()),
        started_at: Utc::now(),
        ended_at: None,
        status: CheckStatus::Down,
        duration_secs: None,
        check_count: 3,
        counts_as_downtime: true,
        error_sample: Some("boom".into()),
        severity: IncidentSeverity::Major,
        public_title: None,
        public_description: None,
        created_at: None,
        updated_at: None,
        updates: vec![update(IncidentStatusPhase::Investigating)],
        regions_down: vec!["us-east".into()],
        regions_up: vec!["eu-helsinki".into()],
    };
    let d = incident_detail(&inc, Some("api".into()));
    assert_eq!(d.state, "down");
    assert_eq!(d.severity, "major");
    assert_eq!(d.monitor_name.as_deref(), Some("api"));
    assert_eq!(d.regions_down, vec!["us-east".to_string()]);
    assert_eq!(d.regions_up, vec!["eu-helsinki".to_string()]);
    assert!(d.resolved_at.is_none());
    assert_eq!(d.error_sample.as_deref(), Some("boom"));
    assert_eq!(d.updates.len(), 1);
    assert_eq!(d.updates[0].phase, "investigating");
}

fn metrics(samples: u64, last_status: &str, last_minute_ts: Option<i64>) -> DashboardMetrics {
    DashboardMetrics {
        target_id: Uuid::nil(),
        samples,
        up: 0,
        avg_ms: 0,
        p50_ms: 0,
        p95_ms: 0,
        last_status: last_status.to_string(),
        last_minute_ts,
    }
}

#[test]
fn current_state_is_no_data_without_samples() {
    assert_eq!(current_state(None, None), "no_data");
    assert_eq!(
        current_state(Some(&metrics(0, "up", None)), None),
        "no_data"
    );
}

#[test]
fn current_state_maps_last_status_with_samples() {
    for s in ["up", "down", "degraded", "error"] {
        assert_eq!(current_state(Some(&metrics(3, s, None)), None), s);
    }
    // An unexpected enum string degrades to no_data rather than leaking it.
    assert_eq!(
        current_state(Some(&metrics(3, "weird", None)), None),
        "no_data"
    );
}

#[test]
fn current_state_prefers_the_folded_status_over_the_last_row() {
    use crate::domain::CheckStatus;
    assert_eq!(
        current_state(Some(&metrics(3, "down", None)), Some(CheckStatus::Degraded)),
        "degraded"
    );
    assert_eq!(
        current_state(Some(&metrics(0, "down", None)), Some(CheckStatus::Degraded)),
        "no_data"
    );
}

#[test]
fn parse_state_accepts_known_rejects_unknown() {
    for s in ["up", "down", "degraded", "error", "no_data"] {
        assert_eq!(parse_state(s).unwrap(), s);
    }
    assert!(parse_state("paused").is_err());
}

#[test]
fn parse_kind_accepts_known_rejects_unknown() {
    for k in crate::domain::CheckSpec::ALL_KINDS {
        assert_eq!(parse_kind(k).unwrap(), k);
    }
    assert!(parse_kind("grpc").is_err());
}

#[test]
fn parse_phase_accepts_known_rejects_unknown() {
    for p in IncidentStatusPhase::ALL {
        assert_eq!(parse_phase(p.as_db_str()).unwrap(), *p);
    }
    assert!(parse_phase("acknowledged").is_err());
}

#[test]
fn ts_to_rfc3339_handles_none_and_epoch() {
    assert_eq!(ts_to_rfc3339(None), None);
    assert_eq!(
        ts_to_rfc3339(Some(0)).as_deref(),
        Some("1970-01-01T00:00:00+00:00")
    );
    assert_eq!(
        ms_to_rfc3339(1_000).as_deref(),
        Some("1970-01-01T00:00:01+00:00")
    );
}

#[test]
fn parse_window_accepts_known_rejects_unknown() {
    for (w, secs) in [
        ("1h", 60u32),
        ("24h", 1_800),
        ("7d", 10_800),
        ("30d", 43_200),
    ] {
        let (span, bucket) = parse_window(w).unwrap();
        assert_eq!(bucket, secs);
        assert!(span.num_hours() > 0);
    }
    assert!(parse_window("90m").is_err());
}

#[test]
fn parse_uuid_rejects_garbage() {
    assert!(parse_uuid("not-a-uuid", "monitor id").is_err());
    assert!(parse_uuid(&Uuid::nil().to_string(), "monitor id").is_ok());
}

fn step(op: &str, outcome: StepOutcome, ms: u32) -> StepTrace {
    StepTrace {
        op: op.into(),
        outcome,
        duration_ms: ms,
    }
}

fn flow_run(stopped: Option<usize>, evidence: Option<FlowEvidence>) -> FlowRunView {
    FlowRunView {
        timestamp: Utc::now(),
        region: "eu-helsinki".into(),
        status: CheckStatus::Down,
        duration_ms: 3_100,
        stopped_step: stopped,
        error: Some("step 2/3 click: selector not found".into()),
        steps: vec![
            step("fill", StepOutcome::Passed, 40),
            step("click", StepOutcome::Failed, 10_000),
            step("assert_url", StepOutcome::Skipped, 0),
        ],
        evidence,
        evidence_expired: false,
    }
}

#[test]
fn a_run_numbers_its_steps_from_one() {
    let item = flow_run_item(flow_run(Some(1), None));
    assert_eq!(item.failed_step, Some(2));
    assert_eq!(
        item.steps.iter().map(|s| s.step).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(item.steps[2].outcome, "skipped");
}

#[test]
fn a_completed_run_names_no_failed_step() {
    assert_eq!(flow_run_item(flow_run(None, None)).failed_step, None);
}

#[test]
fn evidence_carries_the_page_without_its_console() {
    let item = flow_run_item(flow_run(
        Some(1),
        Some(FlowEvidence {
            final_url: Some("https://app.example.com/login".into()),
            title: Some("Sign in".into()),
            text_snippet: Some("Your password is invalid!".into()),
            console: vec![ConsoleLine {
                level: "error".into(),
                text: "boom".into(),
            }],
        }),
    ));
    let evidence = item.evidence.expect("a failure captured the page");
    assert_eq!(
        evidence.text_snippet.as_deref(),
        Some("Your password is invalid!")
    );
    assert!(!serde_json::to_string(&evidence).unwrap().contains("boom"));
}

fn bucket(avg: Option<u32>, samples: u64, failed: u64) -> FlowStepBucket {
    FlowStepBucket {
        t: 0,
        avg,
        samples,
        failed,
    }
}

#[test]
fn a_trend_measures_between_the_slices_that_timed_something() {
    let item = step_trend_item(FlowStepTrend {
        step: 3,
        op: "assert_url".into(),
        buckets: vec![
            bucket(None, 0, 2),
            bucket(Some(200), 5, 0),
            bucket(None, 0, 1),
            bucket(Some(800), 4, 0),
        ],
    });
    assert_eq!(item.step, 4);
    assert_eq!((item.first_ms, item.last_ms), (Some(200), Some(800)));
    assert_eq!(item.change_ratio, Some(4.0));
    assert_eq!((item.samples, item.failed), (9, 3));
}

#[test]
fn sanitize_drops_what_a_reader_could_not_see() {
    let hidden = "ok\u{200b}\u{202e}\u{2069}\u{feff}\u{e0041}";
    assert_eq!(sanitize_data(hidden), "ok");
    assert_eq!(sanitize_data("line\nnext\tcol"), "line\nnext\tcol");
    assert_eq!(sanitize_prompt(hidden), "ok");
}

#[test]
fn a_prompt_says_when_it_is_showing_less_than_the_whole_value() {
    let tags: Vec<String> = (0..40).map(|i| format!("service-{i}")).collect();
    let args = UpdateMonitorArgs {
        tags: Some(tags),
        ..patch_args()
    };
    let (_, changes) = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap();
    let shown = sanitize_prompt(&changes[0].to);
    assert!(shown.ends_with("... (truncated)"), "{shown}");
    assert_eq!(sanitize_prompt("short"), "short");
}

#[test]
fn a_tag_cannot_hide_an_instruction_in_what_comes_back() {
    let args = UpdateMonitorArgs {
        tags: Some(vec!["prod\u{202e}drop everything".into()]),
        ..patch_args()
    };
    let err = build_monitor_patch(&args, &stored_monitor(), &[], 3).unwrap_err();
    assert_eq!(err.code, codes::INVALID_ARGUMENT);

    // A tag stored before the rule still gets scrubbed on its way back out.
    let mut target = stored_monitor();
    target.tags = vec!["prod\u{202e}drop everything".into()];
    let args = UpdateMonitorArgs {
        tags: Some(vec!["prod".into()]),
        ..patch_args()
    };
    let (_, changes) = build_monitor_patch(&args, &target, &[], 3).unwrap();
    assert_eq!(changes[0].from, "proddrop everything");
}

#[test]
fn a_step_that_never_passed_reports_no_ratio() {
    let item = step_trend_item(FlowStepTrend {
        step: 0,
        op: "fill".into(),
        buckets: vec![bucket(None, 0, 3)],
    });
    assert_eq!(
        (item.first_ms, item.last_ms, item.change_ratio),
        (None, None, None)
    );
    assert_eq!(item.failed, 3);
}
