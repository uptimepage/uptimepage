//! Background fan-out of public incident updates to confirmed subscribers.
//! Independent of the escalation engine (gated by plan) so public incidents
//! notify regardless. The claim is an insert into the delivery log, so a crash
//! or a second replica never double-sends.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use uuid::Uuid;

use crate::email::templates::subscriber_maintenance::MaintenanceNotice;
use crate::email::{
    EmailAddress, EmailError, EmailSender, EmailTemplate, SendClass, TransactionalEmail,
};
use crate::http_outbound::{OutboundHttpClient, post_bytes_with_headers};
use crate::storage::subscriber_deliveries::{
    self, INCIDENT_UPDATE, MAINTENANCE, MAINTENANCE_UPDATE,
};
use crate::storage::subscriber_maintenance::{self, PendingMaintenance, PendingMaintenanceUpdate};
use crate::storage::subscribers::{self, PendingUpdate};

const SEND_CONCURRENCY: usize = 8;

/// What to log and store for one delivery, and whether it used an attempt: a
/// send held back by pacing did not.
fn outcome(delivered: anyhow::Result<()>) -> (Option<String>, bool) {
    match delivered {
        Ok(()) => (None, true),
        Err(e) => {
            let pacing = e
                .downcast_ref::<EmailError>()
                .is_some_and(EmailError::is_pacing);
            (Some(format!("{e:#}")), !pacing)
        }
    }
}

pub struct SubscriberDispatchConfig {
    pub tick_interval: Duration,
    pub batch_limit: i64,
    /// Wildcard base domain for `{slug}.{base_domain}` page links; empty on a
    /// self-host deploy, where `public_base_url` is used instead.
    pub base_domain: String,
    pub public_base_url: String,
    /// Whether pages are served on hosts of their own; decides where on the
    /// origin the page itself lives.
    pub subdomain_routes: bool,
    pub unsubscribe_secret: String,
    pub from_address: String,
    pub from_name: String,
}

pub struct SubscriberDispatcher {
    pool: PgPool,
    email: Arc<dyn EmailSender>,
    http: OutboundHttpClient,
    cfg: SubscriberDispatchConfig,
}

impl SubscriberDispatcher {
    pub fn new(
        pool: PgPool,
        email: Arc<dyn EmailSender>,
        http: OutboundHttpClient,
        cfg: SubscriberDispatchConfig,
    ) -> Self {
        Self {
            pool,
            email,
            http,
            cfg,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) {
        let mut ticker = interval(self.cfg.tick_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("subscriber_dispatch: shutdown received");
                    return;
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.tick_incidents().await {
                        tracing::warn!(error = %err, "subscriber_dispatch incident tick failed");
                    }
                    if let Err(err) = self.tick_maintenance().await {
                        tracing::warn!(error = %err, "subscriber_dispatch maintenance tick failed");
                    }
                    if let Err(err) = self.tick_maintenance_updates().await {
                        tracing::warn!(error = %err, "subscriber_dispatch maintenance update tick failed");
                    }
                }
            }
        }
    }

    /// One incident + maintenance sweep. Exposed for tests and manual triggers.
    pub async fn run_once(&self) -> anyhow::Result<()> {
        self.tick_incidents().await?;
        self.tick_maintenance().await?;
        self.tick_maintenance_updates().await?;
        Ok(())
    }

    async fn tick_incidents(&self) -> anyhow::Result<()> {
        let pending = subscribers::list_pending(&self.pool, self.cfg.batch_limit).await?;
        let mut claimed = Vec::new();
        for p in pending {
            if subscriber_deliveries::claim(
                &self.pool,
                p.subscriber_id,
                p.org_id,
                INCIDENT_UPDATE,
                p.update_id,
                "",
            )
            .await?
            {
                claimed.push(p);
            }
        }
        // Sends are network-bound; run them concurrently so one slow recipient
        // can't stall the batch past the tick. Each marks its own outcome.
        stream::iter(claimed)
            .for_each_concurrent(SEND_CONCURRENCY, |p| async move {
                let (error, counts) = outcome(self.deliver_incident(&p).await);
                if let Some(err) = &error {
                    tracing::warn!(error = %err, subscriber = %p.subscriber_id, "subscriber delivery failed");
                }
                if let Err(err) = subscriber_deliveries::mark(
                    &self.pool,
                    p.subscriber_id,
                    INCIDENT_UPDATE,
                    p.update_id,
                    "",
                    error.as_deref(),
                    counts,
                )
                .await
                {
                    tracing::warn!(error = %err, "subscriber mark failed");
                }
            })
            .await;
        Ok(())
    }

    async fn tick_maintenance(&self) -> anyhow::Result<()> {
        let pending =
            subscriber_maintenance::list_pending(&self.pool, self.cfg.batch_limit).await?;
        let mut claimed = Vec::new();
        for m in pending {
            if subscriber_deliveries::claim(
                &self.pool,
                m.subscriber_id,
                m.org_id,
                MAINTENANCE,
                m.maintenance_id,
                &m.phase,
            )
            .await?
            {
                claimed.push(m);
            }
        }
        stream::iter(claimed)
            .for_each_concurrent(SEND_CONCURRENCY, |m| async move {
                let (error, counts) = outcome(self.deliver_maintenance(&m).await);
                if let Some(err) = &error {
                    tracing::warn!(error = %err, subscriber = %m.subscriber_id, "maintenance delivery failed");
                }
                if let Err(err) = subscriber_deliveries::mark(
                    &self.pool,
                    m.subscriber_id,
                    MAINTENANCE,
                    m.maintenance_id,
                    &m.phase,
                    error.as_deref(),
                    counts,
                )
                .await
                {
                    tracing::warn!(error = %err, "maintenance mark failed");
                }
            })
            .await;
        Ok(())
    }

    async fn tick_maintenance_updates(&self) -> anyhow::Result<()> {
        let pending =
            subscriber_maintenance::list_pending_updates(&self.pool, self.cfg.batch_limit).await?;
        let mut claimed = Vec::new();
        for u in pending {
            if subscriber_deliveries::claim(
                &self.pool,
                u.subscriber_id,
                u.org_id,
                MAINTENANCE_UPDATE,
                u.update_id,
                "",
            )
            .await?
            {
                claimed.push(u);
            }
        }
        stream::iter(claimed)
            .for_each_concurrent(SEND_CONCURRENCY, |u| async move {
                let (error, counts) = outcome(self.deliver_maintenance_update(&u).await);
                if let Some(err) = &error {
                    tracing::warn!(error = %err, subscriber = %u.subscriber_id, "maintenance update delivery failed");
                }
                if let Err(err) = subscriber_deliveries::mark(
                    &self.pool,
                    u.subscriber_id,
                    MAINTENANCE_UPDATE,
                    u.update_id,
                    "",
                    error.as_deref(),
                    counts,
                )
                .await
                {
                    tracing::warn!(error = %err, "maintenance update mark failed");
                }
            })
            .await;
        Ok(())
    }

    async fn deliver_incident(&self, p: &PendingUpdate) -> anyhow::Result<()> {
        let origin = self.origin(
            &p.slug,
            p.custom_domain.as_deref(),
            p.custom_domain_published,
        );
        let unsubscribe_url = self.unsubscribe_url(&origin, p.subscriber_id);
        if p.channel == "webhook" {
            let page_url = self.page_url(
                &p.slug,
                p.custom_domain.as_deref(),
                p.custom_domain_published,
            );
            let payload = serde_json::json!({
                "type": "incident_update",
                "incident": { "id": p.incident_id, "title": p.incident_title() },
                "update": { "id": p.update_id, "phase": p.phase, "message": p.message },
                "page": { "name": p.page_name, "url": page_url },
                "incident_url": format!("{origin}/status/incidents/{}", p.incident_id),
                "unsubscribe_url": unsubscribe_url,
            });
            return self
                .post_webhook(&p.target, &payload, p.signing_secret.as_deref())
                .await;
        }
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(self.cfg.from_address.clone(), self.cfg.from_name.clone()),
            to: EmailAddress::new(p.target.clone(), p.target.clone()),
            template: EmailTemplate::SubscriberIncident {
                locale: p.locale(),
                page_name: p.page_name.clone(),
                incident_title: p.incident_title(),
                phase: p.phase.clone(),
                message: p.message.clone(),
                incident_url: format!("{origin}/status/incidents/{}", p.incident_id),
                unsubscribe_url,
            },
        };
        self.send(outgoing).await
    }

    async fn deliver_maintenance(&self, m: &PendingMaintenance) -> anyhow::Result<()> {
        let origin = self.origin(
            &m.slug,
            m.custom_domain.as_deref(),
            m.custom_domain_published,
        );
        let unsubscribe_url = self.unsubscribe_url(&origin, m.subscriber_id);
        let page_url = self.page_url(
            &m.slug,
            m.custom_domain.as_deref(),
            m.custom_domain_published,
        );
        let description = m.description.as_deref().filter(|d| !d.trim().is_empty());
        if m.channel == "webhook" {
            let payload = serde_json::json!({
                "type": "maintenance",
                "maintenance": {
                    "id": m.maintenance_id,
                    "title": m.title,
                    "description": description,
                    "phase": m.phase,
                    "starts_at": m.starts_at,
                    "ends_at": m.ends_at,
                },
                "page": { "name": m.page_name, "url": page_url },
                "unsubscribe_url": unsubscribe_url,
            });
            return self
                .post_webhook(&m.target, &payload, m.signing_secret.as_deref())
                .await;
        }
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(self.cfg.from_address.clone(), self.cfg.from_name.clone()),
            to: EmailAddress::new(m.target.clone(), m.target.clone()),
            template: EmailTemplate::SubscriberMaintenance {
                locale: m.locale(),
                page_name: m.page_name.clone(),
                title: m.title.clone(),
                notice: if m.phase == "completed" {
                    MaintenanceNotice::Completed {
                        description: description.map(str::to_owned),
                    }
                } else {
                    MaintenanceNotice::Scheduled {
                        description: description.map(str::to_owned),
                    }
                },
                starts_at: m.starts_at,
                ends_at: m.ends_at,
                page_url,
                unsubscribe_url,
            },
        };
        self.send(outgoing).await
    }

    async fn deliver_maintenance_update(&self, u: &PendingMaintenanceUpdate) -> anyhow::Result<()> {
        let origin = self.origin(
            &u.slug,
            u.custom_domain.as_deref(),
            u.custom_domain_published,
        );
        let unsubscribe_url = self.unsubscribe_url(&origin, u.subscriber_id);
        let page_url = self.page_url(
            &u.slug,
            u.custom_domain.as_deref(),
            u.custom_domain_published,
        );
        if u.channel == "webhook" {
            let payload = serde_json::json!({
                "type": "maintenance_update",
                "maintenance": {
                    "id": u.maintenance_id,
                    "title": u.title,
                    "starts_at": u.starts_at,
                    "ends_at": u.ends_at,
                },
                "update": { "id": u.update_id, "phase": u.phase, "message": u.message },
                "page": { "name": u.page_name, "url": page_url },
                "unsubscribe_url": unsubscribe_url,
            });
            return self
                .post_webhook(&u.target, &payload, u.signing_secret.as_deref())
                .await;
        }
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(self.cfg.from_address.clone(), self.cfg.from_name.clone()),
            to: EmailAddress::new(u.target.clone(), u.target.clone()),
            template: EmailTemplate::SubscriberMaintenance {
                locale: u.locale(),
                page_name: u.page_name.clone(),
                title: u.title.clone(),
                notice: MaintenanceNotice::Update {
                    message: u.message.clone(),
                    ended: u.phase == "completed",
                },
                starts_at: u.starts_at,
                ends_at: u.ends_at,
                page_url,
                unsubscribe_url,
            },
        };
        self.send(outgoing).await
    }

    async fn send(&self, outgoing: TransactionalEmail) -> anyhow::Result<()> {
        self.email
            .send(outgoing, SendClass::Bulk)
            .await
            .map_err(|e| anyhow::Error::new(e).context("subscriber send"))?;
        Ok(())
    }

    /// POST a JSON payload to a subscriber webhook URL through the SSRF-guarded
    /// outbound client (the connector blocks internal/loopback targets even for
    /// a verified URL). Signed with the subscriber's per-row secret so the
    /// receiver can verify authenticity. A non-2xx or unreachable endpoint
    /// surfaces as an error, so the delivery retries up to the attempts cap.
    async fn post_webhook(
        &self,
        url: &str,
        payload: &serde_json::Value,
        signing_secret: Option<&str>,
    ) -> anyhow::Result<()> {
        let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("bad webhook url: {e}"))?;
        let body = serde_json::to_vec(payload)?;
        let mut headers = std::collections::BTreeMap::new();
        if let Some(secret) = signing_secret {
            let ts = chrono::Utc::now().timestamp();
            headers.insert("X-Uptimepage-Timestamp".to_string(), ts.to_string());
            headers.insert(
                "X-Uptimepage-Signature".to_string(),
                crate::security::mac::webhook_signature(secret, ts, &body),
            );
        }
        post_bytes_with_headers(&self.http, &parsed, body, &headers)
            .await
            .map_err(|e| anyhow::anyhow!("webhook post: {e}"))
    }

    fn origin(&self, slug: &str, custom_domain: Option<&str>, published: bool) -> String {
        crate::public_status::urls::page_origin(
            &self.cfg.base_domain,
            &self.cfg.public_base_url,
            slug,
            custom_domain,
            published,
        )
    }

    /// The page's own address. Subscribers keep it, so it has to be the URL
    /// the page answers on rather than one that redirects there.
    fn page_url(&self, slug: &str, custom_domain: Option<&str>, published: bool) -> String {
        crate::public_status::urls::status_url_for(
            self.cfg.subdomain_routes,
            &self.origin(slug, custom_domain, published),
        )
    }

    fn unsubscribe_url(&self, origin: &str, subscriber_id: Uuid) -> String {
        subscribers::unsubscribe_url(&self.cfg.unsubscribe_secret, origin, subscriber_id)
    }
}
