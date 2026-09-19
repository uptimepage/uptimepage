//! `/auth/discord/start` + `/auth/discord/callback` against live Postgres —
//! the Discord twin of `slack_connect_pg_test`: state minting bound to the
//! session org, single-use consume, the user-cancelled bounce, and the
//! cross-org membership gate. The successful code exchange talks to Discord
//! and is covered by unit tests on the response parsing instead.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_test_app_with_pg, make_user, pg_pool_from_env, unique_slug, with_session};
use tower::ServiceExt;
use uptimepage::auth::oauth_state;
use uptimepage::domain::OrgId;
use uptimepage::domain::credential::DISCORD_CONNECT_PROVIDER;

fn discord_cfg(cfg: &mut uptimepage::config::AppConfig) {
    cfg.discord_oauth.client_id = "1234567890".into();
    cfg.discord_oauth.client_secret = "shh".to_string().into();
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
    let (app, org) = build_test_app_with_pg(pool.clone(), discord_cfg).await;
    let user = make_user(pool, "discord").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org.0)
        .execute(pool)
        .await
        .expect("seed membership");
    (
        with_session(app, user, Some(org), Some("discord-session")),
        org,
    )
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn start_redirects_to_discord_with_org_bound_state() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = member_app(&pool).await;

    let (status, location, _) = get(&app, "/auth/discord/start").await;
    assert!(status.is_redirection(), "{status}");
    let location = location.expect("location header");
    assert!(location.starts_with("https://discord.com/oauth2/authorize?"));
    assert!(location.contains("scope=webhook.incoming"));
    assert!(location.contains("response_type=code"));

    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_states WHERE provider = $1 AND org_id = $2")
            .bind(DISCORD_CONNECT_PROVIDER)
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "state row bound to the session org");
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
        DISCORD_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(org.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let path = format!("/auth/discord/callback?error=access_denied&state={s}");
    let (status, location, _) = get(&app, &path).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(
        location.as_deref(),
        Some("/settings/notifications/new?kind=discord&discord=cancelled")
    );

    // The denied dance burned its state — a replay is rejected.
    let (status, _, _) = get(&app, &path).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn callback_rejects_cross_provider_and_foreign_org_states() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = member_app(&pool).await;

    // A slack_connect state presented to the discord callback is burned and
    // rejected.
    let s = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &s,
        uptimepage::domain::credential::SLACK_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(org.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, _, _) = get(
        &app,
        &format!("/auth/discord/callback?error=access_denied&state={s}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-provider state");

    let (foreign_org,): (uuid::Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Foreign', a.id FROM a RETURNING id",
    )
    .bind(unique_slug("discf"))
    .fetch_one(&pool)
    .await
    .unwrap();
    let s = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &s,
        DISCORD_CONNECT_PROVIDER,
        oauth_state::StateBinding {
            org_id: Some(foreign_org),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, _, _) = get(
        &app,
        &format!("/auth/discord/callback?error=access_denied&state={s}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "not a member of the state org"
    );
}
