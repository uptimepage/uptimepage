//! The public acknowledge link end to end on the in-memory app: the signed
//! link takes the incident, a tampered or lapsed one takes nothing, and a GET
//! only ever offers the confirmation — a prefetch must not silence a live page.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio_util::sync::CancellationToken;
use uptimepage::domain::{IncidentState, NewManualIncident, OrgId};
use uptimepage::security::incident_ack::{LINK_TTL_SECS, link_token, link_url};
use uptimepage::storage::{Actor, IncidentOpsStore};
use uuid::Uuid;

const SECRET: &str = "acknowledge-link-test-secret";
const BASE: &str = "https://app.example.com";

struct Rig {
    app: Router,
    ops: std::sync::Arc<dyn IncidentOpsStore>,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
}

async fn rig() -> Rig {
    let state = common::build_test_app_state(|_| {}).with_incident_ack_secret(SECRET.to_string());
    let ops = state.incident_ops_store.clone();
    let org = common::test_org_id();
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
    Rig {
        app: uptimepage::build_app_router(state, CancellationToken::new()),
        ops,
        org,
        incident_id: incident.id,
        channel_id: Uuid::now_v7(),
    }
}

impl Rig {
    async fn link(&self) -> String {
        let generation = self
            .ops
            .generation(self.org, self.incident_id)
            .await
            .unwrap()
            .expect("incident exists");
        let url = link_url(
            BASE,
            SECRET,
            self.org,
            self.incident_id,
            self.channel_id,
            generation,
            chrono::Utc::now(),
        )
        .expect("a base url and a secret mint a link");
        url.strip_prefix(BASE).expect("link sits on the app").into()
    }

    async fn send(&self, method: &str, path: &str) -> (StatusCode, String) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .into_owned();
        (status, body)
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

use tower::ServiceExt;

#[tokio::test]
async fn a_signed_link_confirms_then_acknowledges() {
    let rig = rig().await;
    let link = rig.link().await;

    // GET offers the button and touches nothing.
    let (status, body) = rig.send("GET", &link).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acknowledge"), "{body}");
    assert_eq!(rig.state().await, IncidentState::Triggered);

    let (status, body) = rig.send("POST", &link).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acknowledged"), "{body}");

    let incident = rig
        .ops
        .get(rig.org, rig.incident_id)
        .await
        .unwrap()
        .expect("incident");
    assert_eq!(incident.state, IncidentState::Acknowledged);
    // Possession is the whole proof, so nobody is credited.
    assert_eq!(incident.acknowledged_by, None);
    assert!(incident.acknowledged_at.is_some());

    // Tapping twice is the same answer, not a second acknowledgement.
    let (status, _) = rig.send("POST", &link).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rig.ops
            .get(rig.org, rig.incident_id)
            .await
            .unwrap()
            .unwrap()
            .acknowledged_at,
        incident.acknowledged_at
    );
}

#[tokio::test]
async fn a_link_that_does_not_verify_takes_nothing() {
    let rig = rig().await;
    let good = rig.link().await;

    let mut cases = vec![
        good.replace("&t=", "&t=00"),
        good.replace(&rig.incident_id.to_string(), &Uuid::now_v7().to_string()),
        good.replace(&rig.channel_id.to_string(), &Uuid::now_v7().to_string()),
        good.replace(&rig.org.0.to_string(), &Uuid::now_v7().to_string()),
        "/incident/ack".to_string(),
    ];

    // A lapsed link, signed correctly for the expiry it carries.
    let stale = chrono::Utc::now().timestamp() - 1;
    cases.push(format!(
        "/incident/ack?o={}&i={}&c={}&g=0&e={stale}&t={}",
        rig.org.0,
        rig.incident_id,
        rig.channel_id,
        link_token(SECRET, rig.org, rig.incident_id, rig.channel_id, 0, stale),
    ));

    // A generation swapped in the query without re-signing.
    cases.push(good.replace("&g=0", "&g=1"));

    for case in cases {
        let (status, _) = rig.send("POST", &case).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "POST {case}");
        let (status, _) = rig.send("GET", &case).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {case}");
        assert_eq!(rig.state().await, IncidentState::Triggered, "{case}");
    }
}

#[tokio::test]
async fn a_deployment_without_the_secret_mints_and_honours_nothing() {
    let state = common::build_test_app_state(|_| {});
    let org = common::test_org_id();
    let incident = state
        .incident_ops_store
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
    let app = uptimepage::build_app_router(state, CancellationToken::new());

    let now = chrono::Utc::now();
    assert!(link_url(BASE, "", org, incident.id, Uuid::now_v7(), 0, now).is_none());

    // A link minted under some other secret is refused, rather than an empty
    // key verifying everything.
    let path = link_url(BASE, SECRET, org, incident.id, Uuid::now_v7(), 0, now)
        .unwrap()
        .replace(BASE, "");
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn the_link_outlives_a_page_but_not_indefinitely() {
    assert_eq!(LINK_TTL_SECS, 7 * 24 * 60 * 60);
}

/// The one an expiry alone cannot catch: the incident resolves and comes back
/// inside the link's lifetime. The alert on the phone is about the outage that
/// ended, so it must not silence the one running now.
#[tokio::test]
async fn a_page_from_a_previous_outage_cannot_silence_the_next_one() {
    let rig = rig().await;
    let old = rig.link().await;

    rig.ops
        .resolve(rig.org, rig.incident_id, Actor::System, None)
        .await
        .expect("resolve");
    rig.ops
        .reopen(rig.org, rig.incident_id, Actor::System, None)
        .await
        .expect("reopen");
    assert_eq!(rig.state().await, IncidentState::Triggered);

    // Not a 2xx: ntfy's one-tap button clears the notification on success, and
    // nothing was acknowledged here.
    let (status, body) = rig.send("POST", &old).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("out of date"), "{body}");
    assert_eq!(
        rig.state().await,
        IncidentState::Triggered,
        "the new outage stays unacknowledged"
    );

    let fresh = rig.link().await;
    assert_ne!(fresh, old);
    let (status, _) = rig.send("POST", &fresh).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rig.state().await, IncidentState::Acknowledged);
}

/// The one-tap button reads only the status code and clears the notification on
/// a 2xx, so an incident that closed before the responder reached it must not
/// report success.
#[tokio::test]
async fn a_resolved_incident_refuses_the_tap_rather_than_reporting_success() {
    let rig = rig().await;
    let link = rig.link().await;
    rig.ops
        .resolve(rig.org, rig.incident_id, Actor::System, None)
        .await
        .expect("resolve");

    let (status, body) = rig.send("POST", &link).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("already resolved"), "{body}");
    assert_eq!(rig.state().await, IncidentState::Resolved);
}
