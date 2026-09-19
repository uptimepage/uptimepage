//! Live-PG assertion that every Rust enum whose values are written into a
//! Postgres column with a closed `CHECK (col IN (…))` constraint stays in
//! lockstep with that constraint's value list. Adding a new variant on the
//! Rust side without the paired migration would otherwise compile, pass the
//! in-memory test stores, and 500 the first INSERT that exercises the new
//! variant in production.
//!
//! Skipped by default; runs under `--include-ignored` once `DATABASE_URL`
//! is set. The test harness auto-applies all Postgres migrations on first
//! connect, so the introspection sees the same constraint defs prod runs.

mod common;

use uptimepage::domain::{
    ActorType, AppTheme, ChannelKind, IncidentEventKind, IncidentOrigin, IncidentSeverity,
    IncidentState, IncidentStatusPhase, IncidentUrgency, IncidentVisibility, NotificationReason,
    NotificationStatus, PublicStyle, SubscriberChannel,
};
use uptimepage::domain::{CredentialAction, CredentialOrigin, OauthProvider};

/// Pull the parenthesised list from a constraint def like
/// `CHECK ((severity = ANY (ARRAY['minor'::text, 'major'::text, 'critical'::text])))`
/// or `CHECK ((severity IN ('minor', 'major', 'critical')))`. Postgres
/// normalises the printed form across versions, so we walk the string for
/// single-quoted tokens rather than matching the surrounding shape.
fn quoted_tokens(def: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = def.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut s = String::new();
        loop {
            match chars.next() {
                Some('\'') => {
                    if chars.peek() == Some(&'\'') {
                        s.push('\'');
                        chars.next();
                    } else {
                        break;
                    }
                }
                Some(c) => s.push(c),
                None => break,
            }
        }
        out.push(s);
    }
    out
}

async fn constraint_def(pool: &sqlx::PgPool, conname: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
    )
    .bind(conname)
    .fetch_optional(pool)
    .await
    .expect("query pg_constraint")
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[tokio::test]
#[ignore]
async fn incidents_severity_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "incidents_severity_check")
        .await
        .expect("incidents_severity_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        IncidentSeverity::ALL
            .iter()
            .map(|s| s.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "incidents.severity CHECK list ({db:?}) drifted from IncidentSeverity ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn incident_updates_phase_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "incident_updates_phase_check")
        .await
        .expect("incident_updates_phase_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        IncidentStatusPhase::ALL
            .iter()
            .map(|p| p.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "incident_updates.phase CHECK list ({db:?}) drifted from IncidentStatusPhase ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn oauth_identities_provider_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "oauth_identities_provider_check")
        .await
        .expect("oauth_identities_provider_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        OauthProvider::ALL
            .iter()
            .map(|p| p.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "oauth_identities.provider CHECK list ({db:?}) drifted from OauthProvider ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn credential_events_action_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "credential_events_action_check")
        .await
        .expect("credential_events_action_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        CredentialAction::ALL
            .iter()
            .map(|a| a.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "credential_events.action CHECK list ({db:?}) drifted from CredentialAction ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn credential_events_origin_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "credential_events_origin_check")
        .await
        .expect("credential_events_origin_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        CredentialOrigin::ALL
            .iter()
            .map(|o| o.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "credential_events.origin CHECK list ({db:?}) drifted from CredentialOrigin ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn channel_link_codes_purpose_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "channel_link_codes_purpose_check")
        .await
        .expect("channel_link_codes_purpose_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        uptimepage::storage::LinkPurpose::ALL
            .iter()
            .map(|p| p.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "channel_link_codes.purpose CHECK list ({db:?}) drifted from LinkPurpose ({rust:?})"
    );
}

/// `oauth_states.provider` must accept every login provider (the dance
/// writes a state row before the callback ever inserts an identity row)
/// plus the connect-purpose providers, which never reach
/// `oauth_identities`.
#[tokio::test]
#[ignore]
async fn oauth_states_provider_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "oauth_states_provider_check")
        .await
        .expect("oauth_states_provider_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        OauthProvider::ALL
            .iter()
            .map(|p| p.as_db_str().to_string())
            .chain(
                uptimepage::domain::credential::CONNECT_PROVIDERS
                    .iter()
                    .map(|p| p.to_string()),
            )
            .collect(),
    );
    assert_eq!(
        db, rust,
        "oauth_states.provider CHECK list ({db:?}) drifted from OauthProvider + CONNECT_PROVIDERS ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn notification_channels_kind_check_matches_rust_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "notification_channels_kind_check")
        .await
        .expect("notification_channels_kind_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(
        ChannelKind::ALL
            .iter()
            .map(|k| k.as_db_str().to_string())
            .collect(),
    );
    assert_eq!(
        db, rust,
        "notification_channels.kind CHECK list ({db:?}) drifted from ChannelKind ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn users_theme_check_matches_app_theme_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "users_theme_check")
        .await
        .expect("users_theme_check missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(AppTheme::ALL.iter().map(|s| (*s).to_string()).collect());
    assert_eq!(
        db, rust,
        "users.theme CHECK list ({db:?}) drifted from AppTheme ({rust:?})"
    );
}

#[tokio::test]
#[ignore]
async fn status_pages_public_style_check_matches_public_style_enum() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "status_page_style_known")
        .await
        .expect("status_page_style_known missing");
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(PublicStyle::ALL.iter().map(|s| (*s).to_string()).collect());
    assert_eq!(
        db, rust,
        "status_pages.public_style CHECK list ({db:?}) drifted from PublicStyle ({rust:?})"
    );
}

/// Shared body for the operational-incident CHECK constraints added in
/// migration 018 — load the live constraint def, compare its quoted token set
/// against the Rust enum's `as_db_str` values.
async fn assert_check_matches(conname: &str, rust: Vec<String>) {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, conname)
        .await
        .unwrap_or_else(|| panic!("{conname} missing"));
    let db = sorted(quoted_tokens(&def));
    let rust = sorted(rust);
    assert_eq!(
        db, rust,
        "{conname} CHECK list ({db:?}) drifted from Rust enum ({rust:?})"
    );
}

fn db_strs<T: Copy>(all: &[T], f: impl Fn(T) -> &'static str) -> Vec<String> {
    all.iter().map(|v| f(*v).to_string()).collect()
}

#[tokio::test]
#[ignore]
async fn incidents_state_check_matches_rust_enum() {
    assert_check_matches(
        "incidents_state_check",
        db_strs(IncidentState::ALL, IncidentState::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incidents_urgency_check_matches_rust_enum() {
    assert_check_matches(
        "incidents_urgency_check",
        db_strs(IncidentUrgency::ALL, IncidentUrgency::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incidents_origin_check_matches_rust_enum() {
    assert_check_matches(
        "incidents_origin_check",
        db_strs(IncidentOrigin::ALL, IncidentOrigin::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incidents_visibility_check_matches_rust_enum() {
    assert_check_matches(
        "incidents_visibility_check",
        db_strs(IncidentVisibility::ALL, IncidentVisibility::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incident_events_kind_check_matches_rust_enum() {
    assert_check_matches(
        "incident_events_kind_check",
        db_strs(IncidentEventKind::ALL, IncidentEventKind::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incident_events_actor_type_check_matches_rust_enum() {
    assert_check_matches(
        "incident_events_actor_type_check",
        db_strs(ActorType::ALL, ActorType::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incident_notifications_status_check_matches_rust_enum() {
    assert_check_matches(
        "incident_notifications_status_check",
        db_strs(NotificationStatus::ALL, NotificationStatus::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn incident_notifications_reason_check_matches_rust_enum() {
    assert_check_matches(
        "incident_notifications_reason_check",
        db_strs(NotificationReason::ALL, NotificationReason::as_db_str),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn targets_default_severity_check_matches_rust_enum() {
    assert_check_matches(
        "targets_default_severity_check",
        db_strs(IncidentSeverity::ALL, IncidentSeverity::as_db_str),
    )
    .await;
}

/// `incidents.status_at_start` is a *subset* of `CheckStatus` (the values
/// that can open an incident — never 'up'), so it can't reuse the enum
/// directly. Pin the literal set so a future migration that adds, say,
/// 'maintenance' must update this test in the same diff.
#[tokio::test]
#[ignore]
async fn incidents_status_at_start_check_is_pinned() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let def = constraint_def(&pool, "incidents_status_at_start_check")
        .await
        .expect("incidents_status_at_start_check missing");
    let db = sorted(quoted_tokens(&def));
    let expected = sorted(vec!["down".into(), "degraded".into(), "error".into()]);
    assert_eq!(
        db, expected,
        "incidents.status_at_start CHECK list ({db:?}) drifted from the pinned set ({expected:?})"
    );
}

#[tokio::test]
#[ignore]
async fn status_page_subscribers_channel_check_matches_rust_enum() {
    assert_check_matches(
        "status_page_subscribers_channel_check",
        db_strs(SubscriberChannel::ALL, SubscriberChannel::as_db_str),
    )
    .await;
}
