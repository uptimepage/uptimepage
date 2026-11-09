//! The alert sources pages: the list, the form, a source's page with its URL,
//! setup and delivery log, and the Team pitch on a plan without sources.

use crate::common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_test_app_with_pg_store, pg_pool_from_env};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::{
    AlertBinding, AlertGrouping, AlertSourceKind, ChannelConfig, NewAlertSource,
    NewNotificationChannel, OrgId, TargetAlerts, WebhookConfig, WriteSource,
};
use uptimepage::storage::alert_sources::{AlertSourceStore, PgAlertSourceStore};
use uptimepage::storage::{NotificationChannelStore, PgNotificationChannelStore};
use uuid::Uuid;

async fn page(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .expect("page");
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("body");
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn move_to_plan(pool: &PgPool, org: OrgId, plan: &str) {
    sqlx::query(
        "UPDATE accounts SET plan_id = $2 \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org.0)
    .bind(plan)
    .execute(pool)
    .await
    .expect("move plan");
}

async fn source(pool: &PgPool, org: OrgId, kind: AlertSourceKind) -> Uuid {
    let channel = PgNotificationChannelStore::new(pool.clone(), None)
        .create(
            org,
            NewNotificationChannel {
                name: "ops".into(),
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
        .id;
    PgAlertSourceStore::new(pool.clone(), None)
        .create(
            org,
            NewAlertSource {
                name: "Prod Alertmanager".into(),
                kind,
                grouping: AlertGrouping::Group,
                escalation_policy_id: None,
                alerts: TargetAlerts(vec![AlertBinding {
                    channel_id: channel,
                }]),
                notify_recovery: true,
                renotify_interval_secs: 3600,
                signing_secret: None,
            },
            WriteSource::Ui,
            None,
            25,
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_team_org_lists_creates_and_reads_its_sources() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) =
        build_test_app_with_pg_store(pool.clone(), |cfg| cfg.quotas.enforce_plan_features = true)
            .await;
    move_to_plan(&pool, org, "team").await;

    let (status, _, html) = page(&app, "/settings/alert-sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(r#"href="/settings/alert-sources/new""#),
        "{html}"
    );
    let (_, _, empty) = page(&app, "/web/partials/settings/alert-sources").await;
    assert!(
        empty.contains(r#"href="/settings/alert-sources/new?kind=grafana""#),
        "an empty list starts from a tool: {empty}"
    );
    let (status, _, form) = page(&app, "/settings/alert-sources/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("data-kind-card"), "a kind to pick");
    let (_, _, picked) = page(&app, "/settings/alert-sources/new?kind=datadog").await;
    assert!(
        picked.contains(r#"value="datadog" checked"#),
        "the tool it was opened for"
    );

    let id = source(&pool, org, AlertSourceKind::Alertmanager).await;
    let (_, _, list) = page(&app, "/web/partials/settings/alert-sources").await;
    assert!(list.contains("Prod Alertmanager"), "{list}");
    assert!(list.contains("webhook:ops"), "who it pages: {list}");
    assert!(
        list.contains("needs attention") && list.contains("waiting · connect"),
        "a source that never sent an alert: {list}"
    );

    let (status, _, detail) = page(&app, &format!("/settings/alert-sources/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail.contains("/hooks/alerts/"), "the URL with its token");
    assert!(detail.contains("alertmanager.yml"), "its tool's setup");
    assert!(detail.contains("nothing received yet"), "an empty log");

    let (status, _, edit) = page(&app, &format!("/settings/alert-sources/{id}/edit")).await;
    assert_eq!(status, StatusCode::OK, "{edit}");
    assert!(
        edit.contains("Prod Alertmanager") && !edit.contains("data-kind-card"),
        "kind fixed"
    );

    let (status, _, _) = page(&app, &format!("/settings/alert-sources/{}", Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_sources_gets_the_team_pitch() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) =
        build_test_app_with_pg_store(pool.clone(), |cfg| cfg.quotas.enforce_plan_features = true)
            .await;
    move_to_plan(&pool, org, "pro").await;

    let (status, _, html) = page(&app, "/settings/alert-sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("on the Team plan"), "{html}");
    assert!(!html.contains(r#"href="/settings/alert-sources/new""#));
    assert!(
        html.contains("with example sources") && html.contains("Prod Alertmanager"),
        "the pitch shows the list with made-up sources: {html}"
    );
    assert!(html.contains(" inert>"), "and none of it can be clicked");
    let (status, location, _) = page(&app, "/settings/alert-sources/new").await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(location.as_deref(), Some("/settings/alert-sources"));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_downgraded_org_keeps_its_sources_under_a_notice() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) =
        build_test_app_with_pg_store(pool.clone(), |cfg| cfg.quotas.enforce_plan_features = true)
            .await;
    source(&pool, org, AlertSourceKind::Grafana).await;
    move_to_plan(&pool, org, "pro").await;

    let (status, _, html) = page(&app, "/settings/alert-sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("These sources are held"), "{html}");
    assert!(
        !html.contains("on the Team plan"),
        "the notice, not the pitch"
    );
    assert!(
        html.contains("/web/partials/settings/alert-sources"),
        "its own list"
    );
    assert!(!html.contains(r#"href="/settings/alert-sources/new""#));
}
