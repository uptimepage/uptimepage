//! HTTP contract for `/api/v1/groups` and the group fields on monitors and
//! incidents, against Postgres.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use uptimepage::domain::{
    ChannelConfig, CheckStatus, EscalationTargetType, NewEscalationPolicy, NewEscalationStep,
    NewEscalationTarget, NewNotificationChannel, OrgId, UserId, WebhookConfig, WriteSource,
};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::{
    EscalationPolicyStore, IncidentOpsStore, NotificationChannelStore, PgEscalationPolicyStore,
    PgIncidentOpsStore, PgNotificationChannelStore, create_org_with_owner,
};

struct Api {
    pool: PgPool,
    org: OrgId,
    user: UserId,
    app: Router,
}

impl Api {
    async fn new(mutate: impl FnOnce(&mut uptimepage::config::AppConfig)) -> Option<Self> {
        let pool = common::pg_pool_from_env().await?;
        let user = common::make_user(&pool, "grp-api").await;
        let org = create_org_with_owner(&pool, user, &common::unique_slug("grp-api"), "O")
            .await
            .unwrap()
            .expect("org")
            .id;
        let app = common::with_session(
            common::build_saas_router_with_pg_cfg(pool.clone(), mutate).await,
            user,
            Some(org),
            Some("grp-api-session"),
        );
        Some(Self {
            pool,
            org,
            user,
            app,
        })
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    async fn channel(&self, name: &str) -> Uuid {
        PgNotificationChannelStore::new(self.pool.clone(), None)
            .create(
                self.org,
                NewNotificationChannel {
                    name: name.into(),
                    config: ChannelConfig::Webhook(WebhookConfig {
                        url: "http://127.0.0.1:1/notify".into(),
                        headers: Default::default(),
                        secret: None,
                    }),
                    enabled: true,
                    auto_bind_tags: Vec::new(),
                    acknowledge_button: false,
                    resolve_button: false,
                },
                WriteSource::Ui,
                i64::MAX,
                None,
            )
            .await
            .unwrap()
            .id
    }

    async fn group(&self, body: Value) -> Value {
        let (status, group) = self.send("POST", "/api/v1/groups", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        group
    }

    async fn monitor(&self, extra: Value) -> Value {
        let (status, target) = self
            .send("POST", "/api/v1/targets", Some(monitor_body(extra)))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{target}");
        target
    }
}

fn monitor_body(extra: Value) -> Value {
    let mut body = json!({
        "name": format!("m-{}", Uuid::now_v7().simple()),
        "check": {
            "type": "http", "url": "https://example.com/", "method": "GET",
            "timeout": 5000, "follow_redirects": false, "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {}, "verify_tls": true
        },
        "interval": 300
    });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_group_round_trips_with_its_members_and_its_delete_ungroups_them() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let ops = api.channel("ops").await;
    let group = api
        .group(json!({
            "name": "  example.com DNS ",
            "alert_as_one": true,
            "alerts": [{ "channel_id": ops }],
            "renotify_interval_secs": 1800,
            "alert_again": false,
            "tags": ["dns", "p0"],
        }))
        .await;
    assert_eq!(group["name"], "example.com DNS");
    assert_eq!(group["member_count"], 0);
    assert!(group["open_incident_id"].is_null());
    let gid = id(&group);

    let empty = api.group(json!({ "name": "Empty" })).await;
    assert_eq!(empty["alert_as_one"], false);
    assert_eq!(empty["alert_again"], true);

    let member = api.monitor(json!({ "group_id": gid })).await;
    assert_eq!(member["group_id"], gid.as_str());
    assert_eq!(member["group_name"], "example.com DNS");

    let (status, list) = api.send("GET", "/api/v1/groups", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<(&str, u64)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["name"].as_str().unwrap(),
                g["member_count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(names, [("Empty", 0), ("example.com DNS", 1)]);

    let (status, renamed) = api
        .send(
            "PATCH",
            &format!("/api/v1/groups/{gid}"),
            Some(json!({ "name": "DNS" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["member_count"], 1);
    assert_eq!(renamed["tags"], json!(["dns", "p0"]));
    let (_, read) = api
        .send("GET", &format!("/api/v1/targets/{}", id(&member)), None)
        .await;
    assert_eq!(read["group_name"], "DNS");

    let (status, taken) = api
        .send(
            "PATCH",
            &format!("/api/v1/groups/{}", id(&empty)),
            Some(json!({ "name": "DNS" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&taken), "GROUP_NAME_TAKEN");

    let (status, _) = api
        .send("DELETE", &format!("/api/v1/groups/{gid}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, gone) = api
        .send("GET", &format!("/api/v1/groups/{gid}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&gone), "GROUP_NOT_FOUND");
    let (_, read) = api
        .send("GET", &format!("/api/v1/targets/{}", id(&member)), None)
        .await;
    assert!(read["group_id"].is_null());
    assert!(read["group_name"].is_null());

    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM org_audit_log WHERE org_id = $1 AND action LIKE 'group.%' \
         ORDER BY occurred_at, id",
    )
    .bind(api.org.0)
    .fetch_all(&api.pool)
    .await
    .unwrap();
    assert_eq!(
        audited,
        [
            "group.created",
            "group.created",
            "group.updated",
            "group.deleted"
        ]
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_group_write_is_refused_before_the_store_when_its_fields_are_bad() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let ops = api.channel("ops").await;
    let cases = [
        (json!({ "name": " \u{3000}" }), "INVALID_GROUP_NAME"),
        (json!({ "name": "x".repeat(51) }), "GROUP_TOO_LONG"),
        (
            json!({ "name": "a", "alerts": [{ "channel_id": ops }, { "channel_id": ops }] }),
            "INVALID_ALERT_CONFIG",
        ),
        (
            json!({ "name": "a", "alerts": [{ "channel_id": Uuid::now_v7() }] }),
            "INVALID_ALERT_CONFIG",
        ),
        (
            json!({ "name": "a", "renotify_interval_secs": 30 }),
            "INVALID_ALERT_CONFIG",
        ),
        (
            json!({ "name": "a", "tags": ["x".repeat(51)] }),
            "TAG_TOO_LONG",
        ),
    ];
    for (body, want) in cases {
        let (status, err) = api.send("POST", "/api/v1/groups", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {err}");
        assert_eq!(code(&err), want, "{body} -> {err}");
    }
    let (status, _) = api
        .send(
            "POST",
            "/api/v1/groups",
            Some(json!({ "name": "a", "escalation_policy_id": Uuid::now_v7() })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "unknown field");

    let group = api.group(json!({ "name": "DNS" })).await;
    let (status, err) = api
        .send(
            "PATCH",
            &format!("/api/v1/groups/{}", id(&group)),
            Some(json!({ "name": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&err), "INVALID_GROUP_NAME");
    let (status, list) = api.send("GET", "/api/v1/groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1, "nothing else was stored");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_monitor_joins_by_id_or_by_name_but_never_both() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let group = api.group(json!({ "name": "DNS" })).await;
    let gid = id(&group);

    let (status, err) = api
        .send(
            "POST",
            "/api/v1/targets",
            Some(monitor_body(
                json!({ "group_id": gid, "group_name": "DNS" }),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(code(&err), "GROUP_ID_AND_NAME");

    let Some(other) = Api::new(|_| {}).await else {
        return;
    };
    let foreign = other.group(json!({ "name": "Theirs" })).await;
    let (status, err) = api
        .send(
            "POST",
            "/api/v1/targets",
            Some(monitor_body(json!({ "group_id": id(&foreign) }))),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(code(&err), "GROUP_NOT_FOUND");

    let by_name = api.monitor(json!({ "group_name": " New group " })).await;
    assert_eq!(by_name["group_name"], "New group");
    let created_by: Option<Uuid> = sqlx::query_scalar(
        "SELECT created_by FROM monitor_groups WHERE org_id = $1 AND name = 'New group'",
    )
    .bind(api.org.0)
    .fetch_one(&api.pool)
    .await
    .unwrap();
    assert_eq!(created_by, Some(api.user.0));

    let tid = id(&by_name);
    let (status, err) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_id": gid, "group_name": null })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&err), "GROUP_ID_AND_NAME");
    let (_, moved) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_id": gid })),
        )
        .await;
    assert_eq!(moved["group_id"], gid.as_str());
    let (_, out) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_id": null })),
        )
        .await;
    assert!(out["group_id"].is_null(), "{out}");
    let (_, back) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_name": "DNS" })),
        )
        .await;
    assert_eq!(back["group_id"], gid.as_str());
    let (_, blank) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_name": " " })),
        )
        .await;
    assert!(blank["group_id"].is_null(), "{blank}");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn members_show_and_filter_by_the_group_tags_only_while_it_alerts_as_one() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let group = api.group(json!({ "name": "DNS", "tags": ["dns"] })).await;
    let gid = id(&group);
    let member = api
        .monitor(json!({ "group_id": gid, "tags": ["web"] }))
        .await;
    let tid = id(&member);
    let listed = |tag: &'static str| {
        let api = &api;
        async move {
            let (_, page) = api
                .send("GET", &format!("/api/v1/targets?tag={tag}"), None)
                .await;
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(member["effective_tags"], json!(["web"]));
    assert_eq!(listed("web").await, std::slice::from_ref(&tid));
    assert!(listed("dns").await.is_empty());

    api.send(
        "PATCH",
        &format!("/api/v1/groups/{gid}"),
        Some(json!({ "alert_as_one": true })),
    )
    .await;
    let (_, read) = api
        .send("GET", &format!("/api/v1/targets/{tid}"), None)
        .await;
    assert_eq!(read["tags"], json!(["web"]), "own tags never change");
    assert_eq!(read["effective_tags"], json!(["dns"]));
    assert_eq!(listed("dns").await, std::slice::from_ref(&tid));
    assert!(listed("web").await.is_empty());
    let (_, tags) = api.send("GET", "/api/v1/tags", None).await;
    let names: Vec<&str> = tags["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["dns"]);

    api.send(
        "PATCH",
        &format!("/api/v1/groups/{gid}"),
        Some(json!({ "alert_as_one": false })),
    )
    .await;
    let (_, read) = api
        .send("GET", &format!("/api/v1/targets/{tid}"), None)
        .await;
    assert_eq!(read["effective_tags"], json!(["web"]));
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_group_policy_needs_on_call_and_its_binding_is_audited() {
    let Some(gated) = Api::new(|cfg| cfg.quotas.enforce_plan_features = true).await else {
        return;
    };
    let group = gated.group(json!({ "name": "DNS" })).await;
    let (status, err) = gated
        .send(
            "PUT",
            &format!("/api/v1/groups/{}/escalation-policy", id(&group)),
            Some(json!({ "policy_id": Uuid::now_v7() })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(code(&err), "ON_CALL_DISABLED");

    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let pager = api.channel("pager").await;
    let policy = PgEscalationPolicyStore::new(api.pool.clone())
        .create(
            api.org,
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![NewEscalationStep {
                    level: 1,
                    delay_secs: 300,
                    targets: vec![NewEscalationTarget {
                        target_type: EscalationTargetType::Channel,
                        user_id: None,
                        schedule_id: None,
                        channel_id: Some(pager),
                    }],
                }],
            },
            i64::MAX,
        )
        .await
        .unwrap();
    let group = api.group(json!({ "name": "DNS" })).await;
    let path = format!("/api/v1/groups/{}/escalation-policy", id(&group));
    for _ in 0..2 {
        let (status, set) = api
            .send("PUT", &path, Some(json!({ "policy_id": policy.id })))
            .await;
        assert_eq!(status, StatusCode::OK, "{set}");
    }
    let (_, read) = api.send("GET", &path, None).await;
    assert_eq!(read["policy_id"], policy.id.to_string());
    let (status, _) = api
        .send(
            "PUT",
            &format!("/api/v1/groups/{}/escalation-policy", Uuid::now_v7()),
            Some(json!({ "policy_id": null })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let audits: Vec<Value> = sqlx::query_scalar(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'group.escalation_policy_set'",
    )
    .bind(api.org.0)
    .fetch_all(&api.pool)
    .await
    .unwrap();
    assert_eq!(audits.len(), 1, "a repeated PUT changes nothing");
    assert!(audits[0]["from"].is_null());
    assert_eq!(audits[0]["to"], policy.id.to_string());
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_group_incident_names_its_group_and_its_members_point_at_it() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let group = api
        .group(json!({ "name": "DNS", "alert_as_one": true }))
        .await;
    let gid = id(&group);
    let member = api.monitor(json!({ "group_id": gid })).await;
    let tid: Uuid = id(&member).parse().unwrap();
    let opened = PgIncidentStore::new(api.pool.clone())
        .insert_open(
            api.org,
            NewOpenIncident {
                target_id: tid,
                started_at: Utc::now() - chrono::Duration::minutes(2),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: Vec::new(),
                regions_up: Vec::new(),
            },
        )
        .await
        .unwrap()
        .expect("opened");
    PgIncidentOpsStore::new(api.pool.clone())
        .attach_member(api.org, opened)
        .await
        .unwrap();

    let (_, group) = api
        .send("GET", &format!("/api/v1/groups/{gid}"), None)
        .await;
    let gi = group["open_incident_id"].as_str().expect("group incident");

    let (status, detail) = api
        .send("GET", &format!("/api/v1/incidents/{gi}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["incident"]["origin"], "group");
    assert_eq!(
        detail["incident"]["group"],
        json!({ "id": gid, "name": "DNS" })
    );
    assert!(detail["incident"]["group_incident_id"].is_null());
    let members = detail["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["id"], opened.to_string());
    assert_eq!(members[0]["group_incident_id"], gi);
    assert!(members[0]["group"].is_null());

    let (_, list) = api.send("GET", "/api/v1/incidents", None).await;
    let rows = list.as_array().unwrap();
    assert!(
        rows.iter()
            .any(|i| i["id"] == opened.to_string() && i["group_incident_id"] == gi)
    );

    let (status, err) = api
        .send(
            "POST",
            &format!("/api/v1/incidents/{opened}/acknowledge"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    assert_eq!(code(&err), "USE_GROUP_INCIDENT");
}

impl Api {
    async fn html(&self, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn the_console_shows_a_group_that_alerts_as_one_and_its_incident() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let own = api.channel("own-pager").await;
    let group = api
        .group(json!({ "name": "example.com DNS", "alert_as_one": true, "tags": ["dns"] }))
        .await;
    let gid = id(&group);
    api.group(json!({ "name": "Spare" })).await;
    let member = api
        .monitor(json!({ "group_id": gid, "alerts": [{ "channel_id": own }] }))
        .await;
    let tid: Uuid = id(&member).parse().unwrap();
    let opened = PgIncidentStore::new(api.pool.clone())
        .insert_open(
            api.org,
            NewOpenIncident {
                target_id: tid,
                started_at: Utc::now() - chrono::Duration::minutes(2),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: Vec::new(),
                regions_up: Vec::new(),
            },
        )
        .await
        .unwrap()
        .expect("opened");
    PgIncidentOpsStore::new(api.pool.clone())
        .attach_member(api.org, opened)
        .await
        .unwrap();
    let (_, read) = api
        .send("GET", &format!("/api/v1/groups/{gid}"), None)
        .await;
    let gi = read["open_incident_id"].as_str().unwrap().to_string();

    let (status, list) = api.html("/targets").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.contains("group alert"), "header chip");
    assert!(list.contains("via group"), "member row chip");
    assert!(list.contains(&format!("/incidents/{gi}")), "incident badge");
    assert!(
        list.contains(&format!("/groups/{gid}")),
        "settings in the menu"
    );
    assert!(list.contains("Spare"), "an empty group is listed");
    assert!(list.contains("No monitors in this group yet"));
    assert!(
        list.contains("Pick the group's channels first"),
        "a group with no channels is switched from its settings"
    );
    assert!(list.contains("turn group alert off"));

    let (status, settings) = api.html(&format!("/groups/{gid}")).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert!(settings.contains("example.com DNS"));
    assert!(
        settings.contains("own-pager"),
        "the member's own channel is listed"
    );
    assert!(settings.contains("muted"));
    assert!(settings.contains("data-nobody-warning"));
    let (status, _) = api.html(&format!("/groups/{}", Uuid::now_v7())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, form) = api.html(&format!("/targets/{tid}/edit")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("Alerts for this monitor go through its group"));

    let (status, incidents) = api.html("/incidents").await;
    assert_eq!(status, StatusCode::OK);
    assert!(incidents.contains(&format!("/incidents/{gi}")));
    assert!(
        incidents.contains(&format!("/incidents/{opened}")),
        "member nested"
    );
    assert_eq!(
        incidents
            .matches(&format!("data-incident-id=\"{opened}\""))
            .count(),
        0,
        "the member has no row or actions of its own"
    );

    let member_name = member["name"].as_str().unwrap();
    let (_, found) = api.html(&format!("/incidents?q={member_name}")).await;
    assert!(
        found.contains(&format!("/incidents/{gi}")),
        "a member's name finds its group incident"
    );
    let counts = PgIncidentOpsStore::new(api.pool.clone())
        .counts_by_state(
            api.org,
            &uptimepage::storage::IncidentOpsFilter {
                top_level: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(counts.triggered, 1, "the tab counts the outage once");

    let (status, detail) = api.html(&format!("/incidents/{opened}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("Part of the"));
    assert!(detail.contains(&format!("/incidents/{gi}")));
    assert!(!detail.contains("data-incident-action=\"acknowledge\""));

    let (status, detail) = api.html(&format!("/incidents/{gi}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("members"));
    assert!(detail.contains(&format!("/incidents/{opened}")));
    assert!(!detail.contains("data-incident-publish"));

    let (status, tab) = api.html(&format!("/targets/{tid}/incidents")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(tab.contains("part of"), "{tab}");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn an_account_holds_as_many_groups_as_its_plan_allows_monitors() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    common::set_account_targets_cap(&api.pool, api.user, 2).await;
    api.group(json!({ "name": "A" })).await;
    let member = api.monitor(json!({ "group_name": "B" })).await;

    let (status, err) = api
        .send("POST", "/api/v1/groups", Some(json!({ "name": "C" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(code(&err), "QUOTA_EXCEEDED");

    let tid = id(&member);
    let (status, err) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_name": "C" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(code(&err), "QUOTA_EXCEEDED");
    let (status, moved) = api
        .send(
            "PATCH",
            &format!("/api/v1/targets/{tid}"),
            Some(json!({ "group_name": "A" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "joining a group that exists is free: {moved}"
    );

    let (status, err) = api
        .send(
            "POST",
            "/api/v1/targets/bulk-action",
            Some(json!({ "ids": [tid], "action": { "type": "set_group", "group": "D" } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    let (status, err) = api
        .send(
            "POST",
            "/api/v1/targets",
            Some(monitor_body(json!({ "group_name": "E" }))),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(code(&err), "QUOTA_EXCEEDED");
    use uptimepage::storage::{MonitorGroupStore, PgMonitorGroupStore, TargetStore};
    let refused = PgMonitorGroupStore::new(api.pool.clone())
        .create(
            api.org,
            uptimepage::domain::NewMonitorGroup::named("F"),
            WriteSource::Ui,
            None,
            2,
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(uptimepage::error::AppError::QuotaExceeded { .. })
        ),
        "{refused:?}"
    );
    let new: uptimepage::domain::NewTarget =
        serde_json::from_value(monitor_body(json!({ "group_name": "G" }))).unwrap();
    let refused = uptimepage::storage::PostgresTargetStore::from_pool(api.pool.clone(), None)
        .create(
            api.org,
            new,
            WriteSource::Ui,
            None,
            uptimepage::domain::WriteCaps {
                targets: 2,
                flow_checks: 2,
                public_components: i64::MAX,
            },
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(uptimepage::error::AppError::QuotaExceeded { .. })
        ),
        "a monitor create holds new groups to the cap too: {refused:?}"
    );
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM monitor_groups WHERE org_id = $1")
        .bind(api.org.0)
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert_eq!(groups, 2);
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_group_section_round_trips_over_the_api() {
    let Some(api) = Api::new(|_| {}).await else {
        return;
    };
    let slug = common::unique_slug("secapi");
    let (status, page) = api
        .send(
            "POST",
            "/api/v1/status-pages",
            Some(json!({ "slug": slug, "name": "Status" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    let pid = id(&page);
    let group = api.group(json!({ "name": "DNS" })).await;
    let gid = id(&group);
    let member = api.monitor(json!({ "group_id": gid })).await;
    let tid = id(&member);

    let path = format!("/api/v1/status-pages/{pid}/sections");
    let (status, section) = api
        .send(
            "POST",
            &path,
            Some(json!({ "group_id": gid, "heading": " Name servers " })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{section}");
    assert_eq!(section["heading"], "Name servers");
    assert_eq!(section["members"], json!([tid]));
    let sid = id(&section);

    let (status, err) = api
        .send("POST", &path, Some(json!({ "group_id": gid })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&err), "SECTION_EXISTS");

    let (_, components) = api
        .send(
            "GET",
            &format!("/api/v1/status-pages/{pid}/components"),
            None,
        )
        .await;
    assert_eq!(components[0]["section_id"], sid.as_str());
    let (status, err) = api
        .send(
            "DELETE",
            &format!("/api/v1/status-pages/{pid}/components/{tid}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&err), "SECTION_MEMBER");

    let (status, _) = api
        .send(
            "PATCH",
            &format!("{path}/{sid}"),
            Some(json!({ "heading": null })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = api.send("GET", &path, None).await;
    assert!(list[0]["heading"].is_null());
    assert_eq!(list[0]["group_name"], "DNS");

    let (status, _) = api.send("DELETE", &format!("{path}/{sid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, components) = api
        .send(
            "GET",
            &format!("/api/v1/status-pages/{pid}/components"),
            None,
        )
        .await;
    assert_eq!(components, json!([]));
    let (status, err) = api.send("DELETE", &format!("{path}/{sid}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&err), "SECTION_NOT_FOUND");
}
