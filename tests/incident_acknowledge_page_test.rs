//! The page an alert's Acknowledge button opens, end to end on the in-memory
//! app: a GET only shows where the incident stands, the POST takes it in the
//! signed-in member's name, and an alert from an earlier episode, or from a
//! channel whose button was switched off since, takes nothing.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::domain::{
    ChannelConfig, IncidentState, NewManualIncident, NewNotificationChannel,
    NotificationChannelUpdate, OrgId, SlackConfig, UserId, WriteSource,
};
use uptimepage::notifier::AckControl;
use uptimepage::storage::{Actor, IncidentOpsStore, NotificationChannelStore};
use uuid::Uuid;

struct Rig {
    app: Router,
    ops: std::sync::Arc<dyn IncidentOpsStore>,
    channels: std::sync::Arc<dyn NotificationChannelStore>,
    org: OrgId,
    user: UserId,
    incident_id: Uuid,
    channel_id: Uuid,
}

async fn slack_channel(channels: &dyn NotificationChannelStore, org: OrgId) -> Uuid {
    channels
        .create(
            org,
            NewNotificationChannel {
                name: "ops-slack".into(),
                config: ChannelConfig::Slack(SlackConfig {
                    webhook_url: "https://hooks.slack.com/services/T/B/ack".into(),
                    mention: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
            },
            WriteSource::Ui,
            10,
            None,
        )
        .await
        .expect("slack channel")
        .id
}

async fn rig() -> Rig {
    let state = common::build_test_app_state(|_| {});
    let ops = state.incident_ops_store.clone();
    let org = common::test_org_id();
    let user = UserId(Uuid::now_v7());
    let incident = ops
        .declare(
            org,
            NewManualIncident {
                title: Some("db unreachable".into()),
                ..Default::default()
            },
            Actor::System,
        )
        .await
        .expect("declare incident");
    let channels = state.notification_channel_store.clone();
    let channel_id = slack_channel(channels.as_ref(), org).await;
    Rig {
        app: common::with_session(
            uptimepage::build_app_router(state, CancellationToken::new()),
            user,
            Some(org),
            None,
        ),
        ops,
        channels,
        org,
        user,
        incident_id: incident.id,
        channel_id,
    }
}

impl Rig {
    fn page(&self, episode: i64) -> String {
        AckControl::page_path(self.org, self.incident_id, self.channel_id, episode)
    }

    async fn send(&self, app: &Router, method: &str, path: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("x-requested-with", "uptimepage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let location = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string());
        let body = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .into_owned();
        (status, location.unwrap_or(body))
    }

    async fn state(&self) -> IncidentState {
        self.ops
            .get(self.org, self.incident_id)
            .await
            .unwrap()
            .expect("incident")
            .state
    }
}

#[tokio::test]
async fn the_page_confirms_then_the_post_acknowledges_as_the_member() {
    let rig = rig().await;
    let page = rig.page(0);

    // GET offers the button and touches nothing: a prefetch must not take it.
    let (status, body) = rig.send(&rig.app, "GET", &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data-incident-acknowledge"), "{body}");
    assert!(body.contains("db unreachable"), "{body}");
    assert_eq!(rig.state().await, IncidentState::Triggered);

    let (status, _) = rig.send(&rig.app, "POST", &page).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let incident = rig
        .ops
        .get(rig.org, rig.incident_id)
        .await
        .unwrap()
        .expect("incident");
    assert_eq!(incident.state, IncidentState::Acknowledged);
    assert_eq!(incident.acknowledged_by, Some(rig.user));

    // The reload says it is theirs now, with no second button.
    let (status, body) = rig.send(&rig.app, "GET", &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("You acknowledged this"), "{body}");
    assert!(!body.contains("data-incident-acknowledge"), "{body}");
}

#[tokio::test]
async fn an_alert_from_before_a_reopen_takes_nothing() {
    let rig = rig().await;
    let page = rig.page(0);
    rig.ops
        .resolve(rig.org, rig.incident_id, Actor::System, None)
        .await
        .unwrap();
    rig.ops
        .reopen(rig.org, rig.incident_id, Actor::System, None)
        .await
        .unwrap();

    let (status, body) = rig.send(&rig.app, "GET", &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("earlier outage"), "{body}");
    assert!(!body.contains("data-incident-acknowledge"), "{body}");

    let (status, _) = rig.send(&rig.app, "POST", &page).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rig.state().await, IncidentState::Triggered);

    // The alert for the episode running now still works.
    let (status, _) = rig.send(&rig.app, "POST", &rig.page(1)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rig.state().await, IncidentState::Acknowledged);
}

#[tokio::test]
async fn a_resolved_incident_has_nothing_to_take() {
    let rig = rig().await;
    rig.ops
        .resolve(rig.org, rig.incident_id, Actor::System, None)
        .await
        .unwrap();
    let (_, body) = rig.send(&rig.app, "GET", &rig.page(0)).await;
    assert!(body.contains("already resolved"), "{body}");
    let (status, _) = rig.send(&rig.app, "POST", &rig.page(0)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rig.state().await, IncidentState::Resolved);
}

/// An org the viewer does not belong to reads exactly like no incident at all.
#[tokio::test]
async fn another_orgs_incident_is_not_found() {
    let rig = rig().await;
    let elsewhere = format!(
        "/incidents/{}/acknowledge?org={}&channel={}&episode=0",
        rig.incident_id,
        Uuid::now_v7(),
        rig.channel_id
    );
    for path in [
        elsewhere.as_str(),
        &format!(
            "/incidents/{}/acknowledge?org={}&channel={}",
            rig.incident_id, rig.org.0, rig.channel_id
        ),
        &format!(
            "/incidents/{}/acknowledge?org={}&episode=0",
            rig.incident_id, rig.org.0
        ),
        &AckControl::page_path(rig.org, Uuid::now_v7(), rig.channel_id, 0),
    ] {
        let (status, _) = rig.send(&rig.app, "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {path}");
        let (status, _) = rig.send(&rig.app, "POST", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "POST {path}");
    }
    assert_eq!(rig.state().await, IncidentState::Triggered);
}

/// Switching the channel's button off, or disabling the channel, withdraws
/// the alerts it already sent; the member can still take it from the console.
#[tokio::test]
async fn a_channel_that_stopped_offering_the_button_withdraws_its_alerts() {
    let rig = rig().await;
    let page = rig.page(0);
    for update in [
        NotificationChannelUpdate {
            acknowledge_button: Some(false),
            ..Default::default()
        },
        NotificationChannelUpdate {
            acknowledge_button: Some(true),
            enabled: Some(false),
            ..Default::default()
        },
    ] {
        rig.channels
            .update(rig.org, rig.channel_id, update, WriteSource::Ui, None)
            .await
            .unwrap()
            .expect("channel");
        let (status, body) = rig.send(&rig.app, "GET", &page).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("button withdrawn"), "{body}");
        assert!(!body.contains("data-incident-acknowledge"), "{body}");
        let (status, _) = rig.send(&rig.app, "POST", &page).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(rig.state().await, IncidentState::Triggered);
    }
    rig.channels
        .delete(rig.org, rig.channel_id, None)
        .await
        .unwrap();
    let (_, body) = rig.send(&rig.app, "GET", &page).await;
    assert!(body.contains("button withdrawn"), "{body}");
    let (status, _) = rig.send(&rig.app, "POST", &page).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rig.state().await, IncidentState::Triggered);
}

/// The channel is looked up in the alert's org only, so a channel id from
/// another org offers nothing here.
#[tokio::test]
async fn a_channel_from_another_org_offers_no_button() {
    let rig = rig().await;
    let theirs = slack_channel(rig.channels.as_ref(), OrgId(Uuid::now_v7())).await;
    let page = AckControl::page_path(rig.org, rig.incident_id, theirs, 0);
    let (_, body) = rig.send(&rig.app, "GET", &page).await;
    assert!(body.contains("button withdrawn"), "{body}");
    let (status, _) = rig.send(&rig.app, "POST", &page).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rig.state().await, IncidentState::Triggered);
}

/// Signing in must bring the reader back to this alert's page, episode and
/// all, not to the console.
#[tokio::test]
async fn a_signed_out_reader_signs_in_and_comes_back_to_the_same_link() {
    let rig = rig().await;
    let app = uptimepage::build_app_router(
        common::build_test_app_state(|_| {}),
        CancellationToken::new(),
    );
    let page = rig.page(0);
    let (status, location) = rig.send(&app, "GET", &page).await;
    assert!(status.is_redirection(), "{status}");
    assert!(location.starts_with("/login?redirect_after="), "{location}");
    assert!(location.contains("episode%3D0"), "{location}");
    assert!(
        location.contains(&format!("channel%3D{}", rig.channel_id)),
        "{location}"
    );
}

async fn insert_org(pool: &sqlx::PgPool) -> OrgId {
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Other Org', a.id FROM a RETURNING id",
    )
    .bind(common::unique_slug("ack"))
    .fetch_one(pool)
    .await
    .expect("insert org");
    OrgId(id)
}

async fn join(pool: &sqlx::PgPool, user: UserId, org: OrgId) {
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(user.0)
        .bind(org.0)
        .execute(pool)
        .await
        .expect("membership");
}

async fn active_org(pool: &sqlx::PgPool, hash: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT active_org_id FROM sessions WHERE id_hash = $1")
        .bind(hash)
        .fetch_one(pool)
        .await
        .expect("session row")
}

/// A member of several orgs acts in the one the alert is about without being
/// moved there: opening the page changes nothing, and "view incident" offers
/// the switch. An org they do not belong to reads as not found.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_alert_from_another_of_the_members_orgs_acts_there_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (app, home, state) = common::build_test_app_with_pg_state(pool.clone(), |_| {}).await;
    let alerted = insert_org(&pool).await;
    let stranger = insert_org(&pool).await;
    let user = common::make_user(&pool, "ackpage").await;
    join(&pool, user, home).await;
    join(&pool, user, alerted).await;
    let hash = format!("ack-page-{}", Uuid::now_v7());
    common::seed_session(&pool, &hash, user, Some(home)).await;
    let app = common::with_session(app, user, Some(home), Some(&hash));
    let declare = |org| {
        let ops = state.incident_ops_store.clone();
        async move {
            ops.declare(
                org,
                NewManualIncident {
                    title: Some("db unreachable".into()),
                    ..Default::default()
                },
                Actor::System,
            )
            .await
            .expect("declare")
            .id
        }
    };
    let channels = state.notification_channel_store.clone();
    let rig = Rig {
        app: app.clone(),
        ops: state.incident_ops_store.clone(),
        channel_id: slack_channel(channels.as_ref(), alerted).await,
        channels,
        org: alerted,
        user,
        incident_id: declare(alerted).await,
    };

    let (status, body) = rig.send(&app, "GET", &rig.page(0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("data-incident-acknowledge"), "{body}");
    assert!(
        body.contains(&format!(r#"data-switch-org="{}""#, alerted.0)),
        "{body}"
    );

    let (status, _) = rig.send(&app, "POST", &rig.page(0)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let taken = rig
        .ops
        .get(alerted, rig.incident_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.acknowledged_by, Some(user));
    assert_eq!(active_org(&pool, &hash).await, Some(home.0));

    let theirs = Rig {
        org: stranger,
        incident_id: declare(stranger).await,
        ..rig
    };
    let (status, _) = theirs.send(&app, "GET", &theirs.page(0)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = theirs.send(&app, "POST", &theirs.page(0)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(active_org(&pool, &hash).await, Some(home.0));
    assert_eq!(theirs.state().await, IncidentState::Triggered);
}
