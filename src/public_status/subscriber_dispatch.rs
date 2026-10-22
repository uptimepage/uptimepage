//! Background delivery of status-page notices to confirmed subscribers.
//! Independent of the escalation engine (gated by plan) so public incidents
//! notify regardless.
//!
//! Each tick plans (one instance at a time) and fans notices out into
//! deliveries. Senders run as workers on every instance: each claims only the
//! next unit it will send itself, keeps its lease alive while the unit is in
//! hand, and writes the outcome as soon as the unit is done.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::email::{EmailAddress, EmailError, EmailSender, MAX_BATCH, MessageId};
use crate::http_outbound::{OutboundHttpClient, post_bytes_with_headers};
use crate::public_status::subscriber_notice::{NoticePayload, PageLinks};
use crate::public_status::subscriber_planner;
use crate::security::sha256_hex;
use crate::storage::locks::try_job;
use crate::storage::subscriber_deliveries::{self, Claimed, Outcome};
use crate::storage::subscriber_notices::{self, EMAIL, WEBHOOK};
use crate::storage::subscribers::FANOUT_MAX_ATTEMPTS;

/// Wait before a failed delivery is tried again.
const RETRY_IN: Duration = Duration::from_secs(20);

pub struct SubscriberDispatchConfig {
    pub tick_interval: Duration,
    /// How long a claimed unit is held without a renewal.
    pub lease: Duration,
    /// How often a held unit's lease is renewed; well inside `lease`.
    pub renew_every: Duration,
    pub email_workers: usize,
    pub webhook_workers: usize,
    pub links: PageLinks,
    pub unsubscribe_secret: String,
    pub from_address: String,
    pub from_name: String,
}

impl SubscriberDispatchConfig {
    pub fn new(
        links: PageLinks,
        unsubscribe_secret: String,
        from_address: String,
        from_name: String,
    ) -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            lease: Duration::from_secs(60),
            renew_every: Duration::from_secs(20),
            email_workers: 8,
            webhook_workers: 16,
            links,
            unsubscribe_secret,
            from_address,
            from_name,
        }
    }
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

    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut workers = JoinSet::new();
        for (channel, count) in [
            (EMAIL, self.cfg.email_workers),
            (WEBHOOK, self.cfg.webhook_workers),
        ] {
            for _ in 0..count {
                workers.spawn(self.clone().work(channel, shutdown.clone()));
            }
        }
        let mut ticker = interval(self.cfg.tick_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => self.tick().await,
            }
        }
        // Each worker finishes the unit in its hands before it stops.
        while workers.join_next().await.is_some() {}
        tracing::debug!("subscriber_dispatch: shutdown complete");
    }

    async fn tick(&self) {
        if let Err(err) = self.plan_once().await {
            tracing::warn!(error = %err, "subscriber_dispatch: planning failed");
        }
    }

    /// One plan, fan-out and send of everything due. For tests and manual
    /// triggers.
    pub async fn run_once(&self) -> anyhow::Result<()> {
        self.plan_once().await?;
        for channel in [EMAIL, WEBHOOK] {
            while self.send_unit(channel).await? {}
        }
        Ok(())
    }

    /// Hands back units whose sender is gone, plans (unless another instance
    /// is planning right now) and fans out, sending nothing.
    pub async fn plan_once(&self) -> anyhow::Result<()> {
        subscriber_deliveries::release_expired(&self.pool).await?;
        let mut planned = Ok(());
        try_job(&self.pool, "subscriber_planner", || async {
            planned = subscriber_planner::plan(&self.pool, &self.cfg.links).await;
        })
        .await;
        // What is already planned still goes out when this pass failed. A
        // notice another instance holds, or whose fan-out fails, waits for
        // the next pass.
        let mut passed = Vec::new();
        while let Some(notice) = subscriber_notices::next_unfanned(&self.pool, &passed).await? {
            match subscriber_notices::fan_out(&self.pool, notice).await {
                Ok(true) => {}
                Ok(false) => passed.push(notice),
                Err(err) => {
                    tracing::warn!(error = %err, %notice, "subscriber_dispatch: fan-out failed");
                    passed.push(notice);
                }
            }
        }
        planned
    }

    async fn work(self: Arc<Self>, channel: &'static str, shutdown: CancellationToken) {
        while !shutdown.is_cancelled() {
            // A unit that panics must not take its worker with it; its rows
            // go back to the queue when their lease runs out.
            let idle = match AssertUnwindSafe(self.send_unit(channel))
                .catch_unwind()
                .await
            {
                Ok(Ok(sent)) => !sent,
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, channel, "subscriber_dispatch: send failed");
                    true
                }
                Err(_) => {
                    tracing::error!(channel, "subscriber_dispatch: a send panicked");
                    true
                }
            };
            if idle {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = sleep(self.cfg.tick_interval) => {}
                }
            }
        }
    }

    /// Claims, sends and records one unit. `false` when nothing was due.
    pub async fn send_unit(&self, channel: &'static str) -> anyhow::Result<bool> {
        let token = Uuid::now_v7();
        let max = if channel == EMAIL {
            MAX_BATCH as i64
        } else {
            1
        };
        let Some((notice, deliveries)) =
            subscriber_deliveries::claim(&self.pool, channel, max, token, self.cfg.lease).await?
        else {
            return Ok(false);
        };
        let outcomes = if !notice.deliverable {
            deliveries
                .iter()
                .map(|d| skipped(d, "the page or the outage no longer calls for it"))
                .collect()
        } else {
            match serde_json::from_value::<NoticePayload>(notice.payload) {
                Err(err) => deliveries
                    .iter()
                    .map(|d| failed(d, format!("unreadable notice {}: {err}", notice.id), true))
                    .collect(),
                Ok(payload) if channel == EMAIL => {
                    self.send_mail(token, &payload, &deliveries).await
                }
                Ok(payload) => self.send_webhook(token, &payload, &deliveries).await,
            }
        };
        subscriber_deliveries::record(&self.pool, token, &outcomes).await?;
        Ok(true)
    }

    async fn send_mail(
        &self,
        token: Uuid,
        payload: &NoticePayload,
        deliveries: &[Claimed],
    ) -> Vec<Outcome> {
        let from = EmailAddress::new(self.cfg.from_address.clone(), self.cfg.from_name.clone());
        let emails = deliveries
            .iter()
            .map(|d| {
                let unsubscribe =
                    payload.unsubscribe_url(&self.cfg.unsubscribe_secret, d.subscriber_id);
                payload.email(from.clone(), &d.target, unsubscribe)
            })
            .collect();
        let batch = match self.email.prepare_batch(emails, batch_key(deliveries)) {
            Ok(batch) => batch,
            Err(err) => {
                return deliveries
                    .iter()
                    .map(|d| failed(d, err.to_string(), true))
                    .collect();
            }
        };
        let sent = self
            .held(token, async {
                // The rows may have gone to another instance while this one
                // waited; then the unit is theirs and nothing is sent here.
                match subscriber_deliveries::renew(&self.pool, token, self.cfg.lease).await {
                    Ok(true) => Some(self.email.send_prepared(batch).await),
                    _ => None,
                }
            })
            .await;
        match sent {
            None => Vec::new(),
            Some(Ok(results)) => {
                let mut results = results.into_iter();
                deliveries
                    .iter()
                    .map(|d| {
                        let result = results.next().unwrap_or_else(|| {
                            Err(EmailError::Transport("no answer for this mail".into()))
                        });
                        mail_outcome(d, result)
                    })
                    .collect()
            }
            Some(Err(failure)) => deliveries
                .iter()
                .map(|d| mail_outcome(d, Err(share_error(&failure.error))))
                .collect(),
        }
    }

    /// A webhook unit is one delivery.
    async fn send_webhook(
        &self,
        token: Uuid,
        payload: &NoticePayload,
        deliveries: &[Claimed],
    ) -> Vec<Outcome> {
        let [d] = deliveries else {
            return deliveries
                .iter()
                .map(|d| failed(d, "a webhook unit holds one delivery".into(), true))
                .collect();
        };
        let unsubscribe = payload.unsubscribe_url(&self.cfg.unsubscribe_secret, d.subscriber_id);
        let Some(body) = payload.webhook(&unsubscribe) else {
            return vec![skipped(d, "not sent to webhooks")];
        };
        let posted = self
            .held(token, async {
                match subscriber_deliveries::renew(&self.pool, token, self.cfg.lease).await {
                    Ok(true) => Some(
                        self.post_webhook(&d.target, &body, d.signing_secret.as_deref())
                            .await,
                    ),
                    _ => None,
                }
            })
            .await;
        match posted {
            None => Vec::new(),
            Some(Ok(())) => vec![sent(d)],
            Some(Err(err)) => vec![failed(d, format!("{err:#}"), true)],
        }
    }

    /// Runs `work` while renewing the unit's lease, so the lease only runs
    /// out when this worker is gone.
    async fn held<T>(&self, token: Uuid, work: impl Future<Output = T>) -> T {
        let renew = async {
            let every = self.cfg.renew_every;
            let mut ticks = interval_at(Instant::now() + every, every);
            loop {
                ticks.tick().await;
                if let Err(err) =
                    subscriber_deliveries::renew(&self.pool, token, self.cfg.lease).await
                {
                    tracing::warn!(error = %err, "subscriber_dispatch: lease renewal failed");
                }
            }
        };
        tokio::select! {
            out = work => out,
            _ = renew => unreachable!("the renewal loop never ends"),
        }
    }

    /// POST a JSON payload to a subscriber webhook URL through the SSRF-guarded
    /// outbound client (the connector blocks internal/loopback targets even for
    /// a verified URL). Signed with the subscriber's per-row secret so the
    /// receiver can verify authenticity.
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
}

/// The same deliveries make the same key, so a resend of one batch is the
/// same request to the provider.
fn batch_key(deliveries: &[Claimed]) -> String {
    let mut ids: Vec<String> = deliveries.iter().map(|d| d.id.to_string()).collect();
    ids.sort();
    format!("notice-batch:{}", sha256_hex(&ids.join(",")))
}

fn mail_outcome(d: &Claimed, result: Result<MessageId, EmailError>) -> Outcome {
    match result {
        Ok(_) => sent(d),
        Err(err) => {
            let counts = !err.is_pacing();
            failed(d, err.to_string(), counts)
        }
    }
}

/// A batch's one error, handed to each of its mails.
fn share_error(err: &EmailError) -> EmailError {
    match err {
        EmailError::RateLimited {
            retry_after,
            detail,
        } => EmailError::RateLimited {
            retry_after: *retry_after,
            detail: detail.clone(),
        },
        EmailError::Deferred { retry_after } => EmailError::Deferred {
            retry_after: *retry_after,
        },
        EmailError::GateUnavailable => EmailError::GateUnavailable,
        other => EmailError::Transport(other.to_string()),
    }
}

fn sent(d: &Claimed) -> Outcome {
    Outcome {
        id: d.id,
        status: "sent",
        error: None,
        attempts: d.attempts,
        retry_in: Duration::ZERO,
    }
}

fn skipped(d: &Claimed, why: &str) -> Outcome {
    Outcome {
        id: d.id,
        status: "skipped",
        error: Some(why.to_owned()),
        attempts: d.attempts,
        retry_in: Duration::ZERO,
    }
}

/// `counts` is false for a send held back by pacing: it goes out again
/// without using up an attempt.
fn failed(d: &Claimed, error: String, counts: bool) -> Outcome {
    let attempts = d.attempts + i32::from(counts);
    tracing::warn!(delivery = %d.id, error = %error, "subscriber delivery failed");
    Outcome {
        id: d.id,
        status: if attempts >= FANOUT_MAX_ATTEMPTS {
            "failed"
        } else {
            "queued"
        },
        error: Some(error),
        attempts,
        retry_in: RETRY_IN,
    }
}
