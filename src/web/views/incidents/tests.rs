use std::collections::HashMap;

use askama::Template;
use chrono::Utc;
use uuid::Uuid;

use super::actors::*;
use super::console::*;
use super::detail::*;
use super::forms::*;
use super::reports::*;
use super::*;
use crate::domain::{
    ActorType, IncidentAcknowledgement, IncidentEvent, IncidentState, OpsIncident, UserId,
};
use crate::web::views::PageSizeLink;

#[test]
fn every_check_kind_has_console_label() {
    for kind in crate::domain::CheckSpec::ALL_KINDS {
        assert!(
            kind == "http" || kind_label(kind) != "http",
            "kind {kind} falls through to the http label"
        );
    }
}

pub(super) fn ops(state: IncidentState) -> OpsIncident {
    OpsIncident {
        id: Uuid::now_v7(),
        target_id: Some(Uuid::now_v7()),
        target_ref: None,
        target_name: None,
        target_kind: None,
        closed_by_monitor_delete: false,
        title: None,
        state,
        severity: crate::domain::IncidentSeverity::Major,
        urgency: crate::domain::IncidentUrgency::High,
        origin: crate::domain::IncidentOrigin::Monitor,
        visibility: crate::domain::IncidentVisibility::Internal,
        paging_enabled: true,
        counts_as_downtime: true,
        started_at: Utc::now(),
        ended_at: None,
        acknowledged_at: None,
        acknowledged_by: None,
        assigned_to: None,
        resolved_by: None,
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
        next_escalation_at: None,
        check_count: 2,
        error_sample: None,
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        recovering_since: None,
    }
}

fn data(rows: Vec<ConsoleRow>) -> ConsoleData {
    ConsoleData {
        rows,
        self_id: Uuid::nil().to_string(),
        limit: 50,
        total: 0,
        page: 1,
        total_pages: 1,
        range_lo: 0,
        range_hi: 0,
        pager_prev: None,
        pager_next: None,
        page_sizes: PAGE_SIZES
            .iter()
            .copied()
            .map(|n| PageSizeLink {
                n,
                href: format!("/incidents?limit={n}"),
                hx_get: None,
                active: n == 50,
            })
            .collect(),
        partial_query: "state=all&limit=50".into(),
    }
}

fn page(rows: Vec<ConsoleRow>) -> IncidentsConsolePage {
    IncidentsConsolePage {
        active_tab: "incidents",
        state_tabs: STATE_FILTERS
            .iter()
            .copied()
            .map(|k| StateTab {
                label: k,
                href: format!("/incidents?state={k}"),
                count: 0,
                active: k == "all",
            })
            .collect(),
        severity_chips: vec![SeverityChip {
            label: "any",
            href: "/incidents".into(),
            active: true,
        }],
        sort_options: SORTS
            .iter()
            .map(|(key, label)| SortOption {
                key,
                label,
                selected: *key == "recent",
            })
            .collect(),
        owner_options: vec![OwnerOption {
            value: String::new(),
            label: "Owner: any".into(),
            selected: true,
        }],
        search: String::new(),
        state_value: "all",
        severity_value: None,
        total: 0,
        data: data(rows),
    }
}

#[test]
fn console_pauses_polling_while_hidden() {
    let html = page(vec![]).render().unwrap();
    // The resume carries the filter guard too: without it a catch-up poll
    // built from pre-click params can land after the filtered swap.
    assert!(html.contains(
        r#"hx-trigger="every 10s [!document.querySelector('#incidents-filter.htmx-request')], sm:poll-resume[!document.querySelector('#incidents-filter.htmx-request')] from:body""#
    ));
    assert!(html.contains("data-poll-pause"));
}

#[test]
fn console_empty_renders_empty_state() {
    let html = page(vec![]).render().unwrap();
    assert!(html.contains("No incidents match"));
}

#[test]
fn console_triggered_row_shows_ack_and_resolve() {
    let row = row_from(
        ops(IncidentState::Triggered),
        Some("api-gateway".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let html = page(vec![row]).render().unwrap();
    assert!(html.contains("api-gateway"));
    assert!(html.contains(r#"data-incident-action="acknowledge""#));
    assert!(html.contains(r#"data-incident-action="resolve""#));
    assert!(!html.contains(r#"data-incident-action="reopen""#));
}

#[test]
fn console_resolved_row_shows_reopen_only() {
    let mut inc = ops(IncidentState::Resolved);
    inc.ended_at = Some(Utc::now());
    let row = row_from(
        inc,
        Some("api".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let html = page(vec![row]).render().unwrap();
    assert!(html.contains(r#"data-incident-action="reopen""#));
    assert!(!html.contains(r#"data-incident-action="acknowledge""#));
}

fn closed_with_deleted_monitor() -> OpsIncident {
    let mut inc = ops(IncidentState::Resolved);
    inc.ended_at = Some(Utc::now());
    inc.target_ref = inc.target_id.take();
    inc.target_name = Some("old-worker".into());
    inc
}

#[test]
fn console_offers_no_reopen_once_the_monitor_is_deleted() {
    let row = row_from(
        closed_with_deleted_monitor(),
        Some("old-worker".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let html = page(vec![row]).render().unwrap();
    assert!(html.contains("monitor deleted"), "{html}");
    assert!(!html.contains(r#"data-incident-action="reopen""#), "{html}");
}

#[test]
fn a_declared_incident_stays_reopenable_after_its_monitor_is_deleted() {
    let mut inc = closed_with_deleted_monitor();
    inc.origin = crate::domain::IncidentOrigin::Manual;
    let row = row_from(
        inc.clone(),
        Some("old-worker".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let console = page(vec![row]).render().unwrap();
    assert!(console.contains("monitor deleted"), "{console}");
    assert!(
        console.contains(r#"data-incident-action="reopen""#),
        "{console}"
    );
    let detail = make_detail_page(
        inc,
        Some("old-worker".into()),
        AckList::default(),
        "old-worker".to_string(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .render()
    .unwrap();
    assert!(detail.contains("old-worker (deleted)"), "{detail}");
    assert!(
        detail.contains(r#"data-incident-action="reopen""#),
        "{detail}"
    );
}

#[test]
fn detail_offers_no_reopen_once_the_monitor_is_deleted() {
    let html = make_detail_page(
        closed_with_deleted_monitor(),
        Some("old-worker".into()),
        AckList::default(),
        "old-worker".to_string(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .render()
    .unwrap();
    assert!(html.contains("old-worker (deleted)"), "{html}");
    assert!(!html.contains(r#"data-incident-action="reopen""#), "{html}");
    assert!(html.contains("data-incident-note"), "{html}");
}

fn alice_acker() -> AckList {
    AckList {
        ackers: vec![AckerView {
            name: "alice@example.com".into(),
            phrase: "by alice@example.com".into(),
            avatar: Some(OwnerAvatar {
                initials: "AL".into(),
                color: "oklch(0.62 0.12 200)".into(),
                label: "alice@example.com".into(),
            }),
            at: Utc::now(),
        }],
        mine: false,
    }
}

#[test]
fn console_shows_acked_by() {
    let mut inc = ops(IncidentState::Acknowledged);
    inc.acknowledged_at = Some(Utc::now());
    let row = row_from(inc, Some("api".into()), alice_acker(), None, None, false);
    let mut p = page(vec![row]);
    p.data.total = 1;
    p.data.range_lo = 1;
    p.data.range_hi = 1;
    let html = p.render().unwrap();
    // Acknowledger shown as an avatar; email only in the tooltip.
    assert!(html.contains("monitors-avatar"));
    assert!(html.contains("acknowledged by alice@example.com"));
}

#[test]
fn acknowledge_stays_on_offer_to_whoever_has_not_yet() {
    let mut inc = ops(IncidentState::Acknowledged);
    inc.acknowledged_at = Some(Utc::now());
    let render = |acked_by_me: bool| {
        let acks = AckList {
            mine: acked_by_me,
            ..alice_acker()
        };
        let row = row_from(inc.clone(), Some("api".into()), acks, None, None, false);
        page(vec![row]).render().unwrap()
    };
    let ack = r#"data-incident-action="acknowledge""#;
    assert!(
        render(false).contains(ack),
        "a manager who has not pressed it yet"
    );
    assert!(!render(true).contains(ack), "alice already did");
    assert!(render(true).contains(r#"data-incident-action="resolve""#));

    let detail = |acked_by_me: bool| {
        let acks = AckList {
            mine: acked_by_me,
            ..alice_acker()
        };
        make_detail_page(inc.clone(), None, acks, "api".into(), vec![], vec![], None)
            .render()
            .unwrap()
    };
    assert!(detail(false).contains(ack));
    assert!(!detail(true).contains(ack));
}

#[test]
fn ackers_are_named_the_same_everywhere() {
    let alice = UserId(Uuid::now_v7());
    let members = HashMap::from([(alice, "alice@example.com".to_string())]);
    let ack = |actor_type, actor_id: Option<UserId>| IncidentAcknowledgement {
        incident_id: Uuid::nil(),
        actor_type,
        actor_id,
        anonymous: actor_id.is_none(),
        at: Utc::now(),
    };
    let deleted = |actor_type| IncidentAcknowledgement {
        anonymous: false,
        ..ack(actor_type, None)
    };
    let views = acker_views(
        &[
            ack(ActorType::User, Some(alice)),
            ack(ActorType::Mcp, Some(alice)),
            ack(ActorType::Link, None),
            ack(ActorType::User, Some(UserId(Uuid::now_v7()))),
            deleted(ActorType::User),
            ack(ActorType::Telegram, Some(alice)),
            ack(ActorType::Telegram, None),
            ack(ActorType::Pushover, None),
            deleted(ActorType::Pushover),
        ],
        &members,
    );
    let phrases: Vec<&str> = views.iter().map(|v| v.phrase.as_str()).collect();
    assert_eq!(
        phrases,
        [
            "by alice@example.com",
            "by alice@example.com via MCP",
            "via notification",
            "by former member",
            "by former member",
            "by alice@example.com via Telegram",
            "via Telegram",
            "via Pushover",
            "by former member via Pushover",
        ]
    );
    assert!(views[0].avatar.is_some());
    assert!(views[2].avatar.is_none());
    assert_eq!(views[2].name, "notification");
    assert_eq!(views[6].name, "Telegram");
    assert!(
        views[5].avatar.is_some(),
        "a linked Telegram account is alice"
    );
    let acks = [ack(ActorType::Mcp, Some(alice)), ack(ActorType::Link, None)];
    assert!(viewer_acknowledged(&acks, alice), "MCP is still alice");
    assert!(!viewer_acknowledged(&acks[1..], alice));
}

#[test]
fn console_row_shows_monitor_kind() {
    let mut row = row_from(
        ops(IncidentState::Triggered),
        Some("api".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    row.kind = Some("tls");
    assert!(page(vec![row]).render().unwrap().contains(">tls<"));

    let manual = row_from(
        ops(IncidentState::Triggered),
        None,
        AckList::default(),
        None,
        None,
        false,
    );
    assert!(page(vec![manual]).render().unwrap().contains("—"));
}

#[test]
fn console_resolved_shows_resolver_avatar_and_auto_marker() {
    let mut inc = ops(IncidentState::Resolved);
    inc.ended_at = Some(Utc::now());
    let resolver = OwnerAvatar {
        initials: "CA".into(),
        color: "oklch(0.62 0.12 50)".into(),
        label: "carol@example.com".into(),
    };
    let row = row_from(
        inc,
        Some("api".into()),
        AckList::default(),
        Some(resolver),
        None,
        false,
    );
    let html = page(vec![row]).render().unwrap();
    assert!(html.contains("resolved by carol@example.com"));

    let mut auto = ops(IncidentState::Resolved);
    auto.ended_at = Some(Utc::now());
    let arow = row_from(
        auto,
        Some("api".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let ahtml = page(vec![arow]).render().unwrap();
    assert!(ahtml.contains(">auto<"));
    assert!(!ahtml.contains("resolved by"));
}

#[test]
fn console_row_shows_assignee_urgency_and_assign_to_me() {
    let mut inc = ops(IncidentState::Triggered);
    inc.urgency = crate::domain::IncidentUrgency::Low;
    let unassigned = row_from(
        inc,
        Some("api".into()),
        AckList::default(),
        None,
        None,
        false,
    );
    let html = page(vec![unassigned]).render().unwrap();
    // Low urgency surfaces, and an unassigned row offers assign-to-me.
    assert!(html.contains("notify"));
    assert!(html.contains(r#"data-incident-assign-self"#));

    let assigned = row_from(
        ops(IncidentState::Triggered),
        Some("api".into()),
        AckList::default(),
        None,
        Some(OwnerAvatar {
            initials: "BO".into(),
            color: "oklch(0.62 0.12 100)".into(),
            label: "bob@example.com".into(),
        }),
        true,
    );
    let html = page(vec![assigned]).render().unwrap();
    // Owner shown as a ringed avatar (mine), email in tooltip, no take button.
    assert!(html.contains("monitors-avatar--me"));
    assert!(html.contains("BO"));
    assert!(html.contains("bob@example.com"));
    assert!(!html.contains(">take<"));
}

#[test]
fn detail_renders_actions_timeline_and_acker() {
    let mut inc = ops(IncidentState::Acknowledged);
    inc.title = Some("Payments degraded".into());
    inc.target_id = None;
    inc.origin = crate::domain::IncidentOrigin::Manual;
    inc.acknowledged_at = Some(Utc::now());
    let timeline = vec![
        TimelineRow {
            kind: "Triggered",
            who: "alice@example.com".to_string(),
            via: None,
            occurred_at: Utc::now(),
            message: None,
        },
        TimelineRow {
            kind: "Acknowledged",
            who: "alice@example.com".to_string(),
            via: Some("Telegram"),
            occurred_at: Utc::now(),
            message: None,
        },
    ];
    let updates = vec![PublicUpdateRow {
        phase: "identified",
        message: "Root cause found.".into(),
        posted_at: Utc::now(),
        author: "bob@example.com".into(),
    }];
    let page = make_detail_page(
        inc,
        None,
        alice_acker(),
        "Payments degraded".to_string(),
        timeline,
        updates,
        None,
    );
    let html = page.render().unwrap();
    assert!(html.contains("Payments degraded"));
    assert!(html.contains(r#"data-incident-note"#));
    assert!(html.contains("activity"));
    assert!(html.contains("alice@example.com"));
    assert!(html.contains(r#"title="Performed through Telegram">telegram</span>"#));
    // Public-update timeline + post form both present, with the author.
    assert!(html.contains(r#"data-incident-update-form"#));
    assert!(html.contains("status updates"));
    assert!(html.contains("Root cause found."));
    assert!(html.contains("bob@example.com"));
    assert!(html.contains("owner"));
    assert!(html.contains(r#"data-incident-assign-select"#));
}

/// The form's defaults are the promise: record it, tell nobody yet.
#[test]
fn declare_form_defaults_to_telling_nobody() {
    let html = DeclareIncidentPage {
        active_tab: "incidents",
        monitors: vec![MonitorOption {
            id: Uuid::now_v7().to_string(),
            name: "api-prod".into(),
        }],
        pages: vec![PageChoice {
            id: Uuid::nil().to_string(),
            name: "Acme customers".into(),
            selected: false,
        }],
    }
    .render()
    .unwrap();
    assert!(
        html.contains(r#"data-incident-page value="00000000-0000-0000-0000-000000000000" class="sr-only">Acme customers"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="notify" value="0" class="sr-only" checked"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="visibility" value="internal" class="sr-only" checked"#),
        "{html}"
    );
    assert!(html.contains("api-prod"), "{html}");
}

/// The only place these can change after declaring, so it has to arrive
/// holding what the incident already says.
#[test]
fn edit_form_arrives_prefilled_and_leaves_the_monitor_alone() {
    let html = EditIncidentPage {
        active_tab: "incidents",
        id: Uuid::now_v7().to_string(),
        title: "partner API degraded".into(),
        title_required: true,
        monitor_name: Some("api-prod".into()),
        target_id: Some(Uuid::now_v7().to_string()),
        severity: "critical",
        urgency: "low",
        visibility: "public",
        public_title: "Elevated errors".into(),
        public_description: "Some checkouts fail.".into(),
        downtime_editable: true,
        counts_as_downtime: false,
    }
    .render()
    .unwrap();
    assert!(html.contains(r#"value="partner API degraded""#), "{html}");
    assert!(
        html.contains(r#"name="title" type="text" required"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="severity" value="critical" checked"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="counts_as_downtime" value="0" class="sr-only" checked"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="urgency" value="low" class="sr-only" checked"#),
        "{html}"
    );
    assert!(html.contains("Elevated errors"), "{html}");
    assert!(html.contains("Some checkouts fail."), "{html}");
    // Context, not a field: rebinding collides with the open-incident rule.
    assert!(!html.contains(r#"name="target_id""#), "{html}");
    assert!(html.contains("fixed after declaring"), "{html}");
}

/// Nothing else reconciles a passing monitor against an incident still
/// open over it.
#[test]
fn detail_offers_to_close_an_incident_whose_monitor_recovered() {
    let mut page = make_detail_page(
        ops(IncidentState::Triggered),
        Some("api".into()),
        AckList::default(),
        "api".to_string(),
        vec![],
        vec![],
        None,
    );
    assert!(
        !page.render().unwrap().contains("still open"),
        "no claim without evidence the monitor is passing"
    );
    page.monitor_recovered_at = Some(Utc::now());
    let html = page.render().unwrap();
    assert!(html.contains("still open"), "{html}");
    assert!(
        html.contains("every region's latest check passed"),
        "{html}"
    );
}

/// The first row answers what a review asks: how long, how fast anyone
/// took it, how loud it was, how much broke.
#[test]
fn detail_leads_with_how_long_how_fast_how_loud() {
    let mut inc = ops(IncidentState::Resolved);
    inc.started_at = Utc::now() - chrono::Duration::minutes(30);
    inc.acknowledged_at = Some(inc.started_at + chrono::Duration::minutes(5));
    inc.ended_at = Some(inc.started_at + chrono::Duration::minutes(22));
    inc.check_count = 7;
    let page = make_detail_page(
        inc,
        Some("api".into()),
        alice_acker(),
        "api".to_string(),
        vec![],
        vec![],
        None,
    );
    let html = page.render().unwrap();
    assert!(html.contains("lasted"), "{html}");
    assert!(html.contains("22m 0s"), "{html}");
    assert!(html.contains("5m 0s"), "acknowledged in: {html}");
    assert!(html.contains(">7</p>"), "failed checks: {html}");
    // Nothing paged: that reads as quiet, not as a measured zero.
    assert!(html.contains("dashboard-rail__value--muted"), "{html}");
}

#[test]
fn detail_renders_delivery_log_with_dead_letter_and_retry() {
    let mut page = make_detail_page(
        ops(IncidentState::Triggered),
        Some("api".into()),
        AckList::default(),
        "api".to_string(),
        vec![],
        vec![],
        None,
    );
    page.notifications = vec![
        NotificationRow {
            channel: "Ops Slack".into(),
            transport: "slack".into(),
            reason: "opened",
            status: "sent",
            status_label: "sent",
            attempt: 1,
            error: None,
            sent_at: Some(Utc::now()),
            next_attempt_at: None,
            dead_lettered: false,
        },
        NotificationRow {
            channel: "Pager webhook".into(),
            transport: "webhook".into(),
            reason: "opened",
            status: "failed",
            status_label: "failed",
            attempt: 5,
            error: Some("connection refused".into()),
            sent_at: None,
            next_attempt_at: None,
            dead_lettered: true,
        },
    ];
    let html = page.render().unwrap();
    assert!(html.contains("delivery"));
    assert!(html.contains("Ops Slack"));
    assert!(html.contains("Pager webhook"));
    assert!(html.contains("dead-letter"));
    assert!(html.contains("connection refused"));
}

#[test]
fn author_label_resolves_member_system_and_departed() {
    let u = UserId(Uuid::now_v7());
    let mut members = HashMap::new();
    members.insert(u, "alice@example.com".to_string());
    assert_eq!(
        author_label(Some(&u.0.to_string()), &members),
        "alice@example.com"
    );
    assert_eq!(author_label(Some("system"), &members), "system");
    assert_eq!(author_label(None, &members), "system");
    let gone = UserId(Uuid::now_v7());
    assert_eq!(
        author_label(Some(&gone.0.to_string()), &members),
        "former member"
    );
}

#[test]
fn detail_internal_shows_publish_public_shows_unpublish() {
    let internal = make_detail_page(
        ops(IncidentState::Triggered),
        None,
        AckList::default(),
        "x".into(),
        vec![],
        vec![],
        None,
    );
    let html = internal.render().unwrap();
    assert!(html.contains(r#"data-incident-publish"#));
    assert!(!html.contains(r#"data-incident-unpublish"#));

    let mut pubinc = ops(IncidentState::Triggered);
    pubinc.visibility = crate::domain::IncidentVisibility::Public;
    let public = make_detail_page(
        pubinc,
        None,
        AckList::default(),
        "x".into(),
        vec![],
        vec![],
        None,
    );
    let html = public.render().unwrap();
    assert!(html.contains(r#"data-incident-unpublish"#));
    assert!(!html.contains(r#"data-incident-publish"#));
}

#[test]
fn actor_label_resolves_who_and_mcp() {
    use crate::domain::{ActorType, IncidentEventKind};
    let u = UserId(Uuid::now_v7());
    let mut members = HashMap::new();
    members.insert(u, "alice@example.com".to_string());
    let ev = |actor_type, actor_id| IncidentEvent {
        id: Uuid::now_v7(),
        incident_id: Uuid::now_v7(),
        occurred_at: Utc::now(),
        kind: IncidentEventKind::Acknowledged,
        actor_type,
        actor_id,
        detail: serde_json::Value::Null,
        message: None,
    };
    assert_eq!(
        actor_label(&ev(ActorType::System, None), &members),
        ("system".into(), None)
    );
    assert_eq!(
        actor_label(&ev(ActorType::User, Some(u)), &members),
        ("alice@example.com".into(), None)
    );
    assert_eq!(
        actor_label(&ev(ActorType::Mcp, Some(u)), &members),
        ("alice@example.com".into(), Some("MCP"))
    );
    // Names nobody, and must not borrow one from a stray actor_id.
    assert_eq!(
        actor_label(&ev(ActorType::Link, None), &members),
        ("notification".into(), None)
    );
    assert_eq!(
        actor_label(&ev(ActorType::Link, Some(u)), &members),
        ("notification".into(), None)
    );
    // An app names a member only through an account they linked.
    for app in crate::domain::LinkedApp::ALL {
        assert_eq!(
            actor_label(&ev(app.actor_type(), Some(u)), &members),
            ("alice@example.com".into(), Some(app.label())),
            "{app:?}"
        );
    }
    assert_eq!(
        actor_label(&ev(ActorType::Pushover, None), &members),
        ("Pushover".into(), None)
    );
    // An actor who has left the org no longer resolves to an email.
    let gone = UserId(Uuid::now_v7());
    assert_eq!(
        actor_label(&ev(ActorType::User, Some(gone)), &members),
        ("former member".into(), None)
    );
    // A deleted account's id is nulled out; it was still a member.
    assert_eq!(
        actor_label(&ev(ActorType::User, None), &members),
        ("former member".into(), None)
    );
}

#[test]
fn detail_shows_write_vs_edit_postmortem() {
    let none = make_detail_page(
        ops(IncidentState::Resolved),
        None,
        AckList::default(),
        "x".into(),
        vec![],
        vec![],
        None,
    );
    assert!(none.render().unwrap().contains("write postmortem"));

    let pm = crate::domain::IncidentPostmortem {
        incident_id: Uuid::now_v7(),
        summary: Some("s".into()),
        root_cause: None,
        impact: None,
        action_items: vec![],
        author_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        published_at: Some(Utc::now()),
    };
    let with = make_detail_page(
        ops(IncidentState::Resolved),
        None,
        AckList::default(),
        "x".into(),
        vec![],
        vec![],
        Some(&pm),
    );
    let html = with.render().unwrap();
    assert!(html.contains("edit postmortem"));
    assert!(html.contains("published"));
}

#[test]
fn fmt_secs_humanises() {
    assert_eq!(fmt_secs(None), None);
    assert_eq!(fmt_secs(Some(8.0)).as_deref(), Some("8s"));
    assert_eq!(fmt_secs(Some(312.0)).as_deref(), Some("5m 12s"));
    assert_eq!(fmt_secs(Some(3780.0)).as_deref(), Some("1h 3m"));
}

#[test]
fn reports_page_renders_kpis_and_top_monitors() {
    let page = IncidentsReportPage {
        active_tab: "incidents",
        window_days: 30,
        windows: WINDOW_DAYS
            .iter()
            .map(|d| WindowOption {
                days: *d,
                active: *d == 30,
            })
            .collect(),
        total: 4,
        mtta: Some("5m 0s".into()),
        mttr: Some("1h 2m".into()),
        by_severity: vec![ReportBucket {
            label: "major".into(),
            count: 3,
        }],
        by_state: vec![ReportBucket {
            label: "resolved".into(),
            count: 3,
        }],
        auto_resolved: 1,
        human_resolved: 1,
        closed_with_monitor: 1,
        top_monitors: vec![
            ReportMonitorRow {
                id: Some(Uuid::now_v7().to_string()),
                name: "api-gateway".into(),
                count: 2,
            },
            ReportMonitorRow {
                id: None,
                name: "old-worker".into(),
                count: 1,
            },
        ],
    };
    let html = page.render().unwrap();
    assert!(html.contains("5m 0s"));
    assert!(html.contains("1h 2m"));
    assert!(html.contains("api-gateway"));
    // Every resolved incident is accounted for.
    assert!(
        html.contains("1 closed when their monitor was deleted"),
        "{html}"
    );
    // A deleted monitor is still named, with nothing to link to.
    assert!(html.contains("old-worker (deleted)"), "{html}");
    assert_eq!(html.matches("href=\"/targets/").count(), 1, "{html}");
    // Shares the dashboard's range tabs, so labels are bare keys.
    assert!(html.contains("range-tabs__btn"), "{html}");
    assert!(html.contains(">7d</a>"), "{html}");
}

#[test]
fn postmortem_form_renders_fields_and_publish() {
    let page = PostmortemFormPage {
        active_tab: "incidents",
        incident_id: Uuid::now_v7().to_string(),
        incident_label: "Payments degraded".into(),
        exists: true,
        published: false,
        summary: "cache stampede".into(),
        root_cause: String::new(),
        impact: String::new(),
        action_items: vec![ActionItemModel {
            text: "add jitter".into(),
            owner_user_id: String::new(),
            done: false,
        }],
        members: vec![MemberChoice {
            id: Uuid::now_v7().to_string(),
            email: "alice@example.com".into(),
        }],
    };
    let html = page.render().unwrap();
    assert!(html.contains("cache stampede"));
    assert!(html.contains("add jitter"));
    assert!(html.contains("alice@example.com"));
    assert!(html.contains(r#"data-postmortem-publish="true""#));
    // A draft that reads as published is the expensive mistake here.
    assert!(html.contains("check-type-card--on"), "{html}");
    assert!(html.contains("yours alone"), "{html}");
    // The saved row and the clone <template> render from one macro, so a
    // row the operator adds gets the same combobox as the ones on load.
    assert_eq!(html.matches("data-ai-owner data-sm-combobox").count(), 2);
}

/// It cannot be published before it exists, so the button must be absent
/// rather than fail on click, and the card must say why it is out of reach.
#[test]
fn postmortem_form_offers_publish_only_once_there_is_something_to_publish() {
    let page = PostmortemFormPage {
        active_tab: "incidents",
        incident_id: Uuid::now_v7().to_string(),
        incident_label: "Payments degraded".into(),
        exists: false,
        published: false,
        summary: String::new(),
        root_cause: String::new(),
        impact: String::new(),
        action_items: vec![],
        members: vec![],
    };
    let html = page.render().unwrap();
    assert!(!html.contains("data-postmortem-publish"), "{html}");
    assert!(html.contains(r#"card-badge--warn">save first"#), "{html}");
}
