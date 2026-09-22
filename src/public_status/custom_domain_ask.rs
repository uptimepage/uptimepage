//! On-demand TLS authorization for Caddy. Binds its own address on the
//! internal network: a path on the operator host would be reachable through
//! `app.{base}` whatever it is called, and the unrecognised-host default-deny
//! would reject an internal hostname anyway.
//!
//! Not an issuance counter. Caddy caches certificates and backs off failed
//! issuance itself, so a per-domain cooldown here would refuse legitimate
//! certificate management.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::request::custom_domains::{CustomDomains, normalize};

pub const ASK_PATH: &str = "/custom-domain/ask";

#[derive(Deserialize)]
struct Ask {
    domain: String,
}

/// Caddy issues on 2xx only. A request without a `domain` is rejected by the
/// extractor as a 400, which refuses issuance like any other unrecognised ask.
async fn ask(State(domains): State<Arc<CustomDomains>>, Query(q): Query<Ask>) -> StatusCode {
    match domains.lookup(&q.domain) {
        Some(_) => {
            metrics::counter!(crate::metric_names::CUSTOM_DOMAIN_ASK, "outcome" => "allowed")
                .increment(1);
            StatusCode::OK
        }
        None => {
            // Debug, not info: the name is the client's own SNI and Caddy asks
            // once per handshake, so a flood would write a line each.
            metrics::counter!(crate::metric_names::CUSTOM_DOMAIN_ASK, "outcome" => "refused")
                .increment(1);
            tracing::debug!(
                domain = %normalize(&q.domain).unwrap_or_default(),
                "refused a certificate for an unserved domain"
            );
            StatusCode::FORBIDDEN
        }
    }
}

pub fn router(domains: Arc<CustomDomains>) -> Router {
    Router::new()
        .route(ASK_PATH, get(ask))
        .with_state(domains)
        .fallback(|| async { StatusCode::NOT_FOUND })
}

/// `None` when no bind address is configured. A bind failure is returned, not
/// logged: silently not answering fails every customer handshake.
pub async fn spawn(
    bind: &str,
    domains: Arc<CustomDomains>,
    shutdown: CancellationToken,
) -> std::io::Result<Option<tokio::task::JoinHandle<()>>> {
    let bind = bind.trim();
    if bind.is_empty() {
        return Ok(None);
    }
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(
        // SAFE: operator bind address, not a peer/user IP
        addr = %bind,
        "custom domain ask listening"
    );
    Ok(Some(tokio::spawn(async move {
        let serve = axum::serve(listener, router(domains))
            .with_graceful_shutdown(async move { shutdown.cancelled().await });
        if let Err(err) = serve.await {
            tracing::error!(?err, "custom domain ask server error");
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OrgId, PageRef, StatusPageId};
    use crate::request::custom_domains::CustomDomainRow;
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    fn served(domain: &str) -> Arc<CustomDomains> {
        let d = Arc::new(CustomDomains::new("example.com"));
        d.install(vec![CustomDomainRow {
            domain: domain.into(),
            page: PageRef {
                page: StatusPageId(uuid::Uuid::from_u128(1)),
                org: OrgId(uuid::Uuid::from_u128(2)),
            },
            slug: "page".into(),
            activated: false,
        }]);
        d
    }

    async fn get_status(domains: Arc<CustomDomains>, uri: &str) -> StatusCode {
        router(domains)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn allows_a_served_domain() {
        let d = served("status.acme.test");
        assert_eq!(
            get_status(d, "/custom-domain/ask?domain=status.acme.test").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn refuses_a_domain_the_snapshot_does_not_serve() {
        let d = served("status.acme.test");
        for domain in ["evil.test", "status.other.test", "app.example.com", ""] {
            assert_eq!(
                get_status(d.clone(), &format!("/custom-domain/ask?domain={domain}")).await,
                StatusCode::FORBIDDEN,
                "{domain}"
            );
        }
    }

    #[tokio::test]
    async fn normalises_the_domain_caddy_asks_about() {
        let d = served("status.acme.test");
        for domain in ["STATUS.acme.test", "status.acme.test."] {
            assert_eq!(
                get_status(d.clone(), &format!("/custom-domain/ask?domain={domain}")).await,
                StatusCode::OK,
                "{domain}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_snapshot_allows_nothing() {
        let d = Arc::new(CustomDomains::new("example.com"));
        assert_eq!(
            get_status(d, "/custom-domain/ask?domain=status.acme.test").await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn nothing_else_answers_on_this_listener() {
        let d = served("status.acme.test");
        for uri in ["/", "/custom-domain/ask", "/login", "/healthz", "/metrics"] {
            let status = get_status(d.clone(), uri).await;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
                "{uri} answered {status}"
            );
        }
    }
}
