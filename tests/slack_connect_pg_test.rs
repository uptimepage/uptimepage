//! `/auth/slack/start` + `/auth/slack/callback` against live Postgres:
//! state minting bound to the session org, single-use consume, the
//! user-cancelled bounce, and the cross-org membership gate. The successful
//! code exchange talks to Slack and is covered by unit tests on the
//! response parsing instead.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_test_app_with_pg, make_user, pg_pool_from_env, unique_slug, with_session};
use tower::ServiceExt;
use uptimepage::auth::oauth_state;
use uptimepage::domain::OrgId;
use uptimepage::domain::credential::SLACK_CONNECT_PROVIDER;

fn slack_cfg(cfg: &mut uptimepage::config::AppConfig) {
    cfg.slack_oauth.client_id = "123.456".into();
    cfg.slack_oauth.client_secret = "shh".to_string().into();
}

async fn get(app: &Router, path: &str) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = String::from_utf8_lossy(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .into_owned();
    (status, location, body)
}

async fn member_app(pool: &sqlx::PgPool) -> (Router, OrgId) {
    let (app, org) = build_test_app_with_pg(pool.clone(), slack_cfg).await;
    let user = make_user(pool, "slack").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org.0)
        .execute(pool)
        .await
        .expect("seed membership");
    (
        with_session(app, user, Some(org), Some("slack-session")),
        org,
    )
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn start_redirects_to_slack_with_org_bound_state() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = member_app(&pool).await;

    let (status, location, _) = get(&app, "/auth/slack/start").await;
    assert!(status.is_redirection(), "{status}");
    let location = location.expect("location header");
    assert!(location.starts_with("https://slack.com/oauth/v2/authorize?"));
    assert!(location.contains("scope=incoming-webhook"));
    assert!(location.contains("redirect_uri="));

    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_states WHERE provider = $1 AND org_id = $2")
            .bind(SLACK_CONNECT_PROVIDER)
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "state row bound to the session org");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn start_json_variant_returns_authorize_url() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = member_app(&pool).await;

    let (status, _, body) = get(&app, "/auth/slack/start?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let url = json["url"].as_str().unwrap();
    assert!(url.starts_with("https://slack.com/oauth/v2/authorize?"));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn callback_rejects_unknown_state() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = member_app(&pool).await;

    let (status, _, _) = get(&app, "/auth/slack/callback?code=x&state=bogus").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn callback_cancel_bounces_to_form_and_burns_state() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = member_app(&pool).await;

    let s = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &s,
        SLACK_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(org.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let path = format!("/auth/slack/callback?error=access_denied&state={s}");
    let (status, location, _) = get(&app, &path).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(
        location.as_deref(),
        Some("/settings/notifications/new?kind=slack&slack=cancelled")
    );

    // The denied dance burned its state — a replay is rejected.
    let (status, _, _) = get(&app, &path).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn callback_rejects_state_minted_for_a_foreign_org() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = member_app(&pool).await;

    let (foreign_org,): (uuid::Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Foreign', a.id FROM a RETURNING id",
    )
    .bind(unique_slug("slackf"))
    .fetch_one(&pool)
    .await
    .unwrap();

    let s = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &s,
        SLACK_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(foreign_org),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (status, _, _) = get(
        &app,
        &format!("/auth/slack/callback?error=access_denied&state={s}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "not a member of the state org"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn delegate_state_callback_needs_no_session_and_cancel_keeps_the_link() {
    use uptimepage::storage::{ChannelLinkCodeStore, LinkPurpose, PgChannelLinkCodeStore};

    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // No session layer at all: the delegate state is the whole authority.
    let (app, org) = common::build_test_app_with_pg(pool.clone(), slack_cfg).await;

    let links = PgChannelLinkCodeStore::new(pool.clone());
    let code_hash = uptimepage::security::sha256_hex(&unique_slug("slack-delegate"));
    let minted = match links
        .mint(
            org,
            LinkPurpose::Delegate,
            None,
            &code_hash,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::days(7),
            5,
        )
        .await
        .unwrap()
    {
        uptimepage::storage::MintOutcome::Created(c) => c,
        uptimepage::storage::MintOutcome::LimitReached => panic!("unexpected limit"),
    };

    let s = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &s,
        SLACK_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(org.0),
            link_code_id: Some(minted.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (status, location, _) = get(
        &app,
        &format!("/auth/slack/callback?error=access_denied&state={s}"),
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(location.as_deref(), Some("/c/done?slack=cancelled"));

    // The cancelled dance must not spend the delegate link.
    assert!(
        links
            .peek(LinkPurpose::Delegate, &code_hash)
            .await
            .unwrap()
            .is_some(),
        "cancel must keep the delegate link alive"
    );
}
