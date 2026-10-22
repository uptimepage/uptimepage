//! Background delivery of status-page notices to confirmed subscribers.
//! Independent of the escalation engine (gated by plan) so public incidents
//! notify regardless.
//!
//! Each tick plans (one instance at a time) and fans notices out into
//! deliveries. Senders run as workers on every instance: each claims only the
//! next unit it will send itself, keeps its lease alive while the unit is in
//! hand, and writes the outcome as soon as the unit is done.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::FutureExt;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::email::{BatchResult, EmailAddress, EmailError, EmailSender, MAX_BATCH, PreparedBatch};
use crate::http_outbound::{OutboundHttpClient, PostAnswer, post_json_for_answer};
use crate::metric_names;
use crate::public_status::subscriber_notice::{NoticePayload, PageLinks};
use crate::public_status::subscriber_planner;
use crate::public_status::subscriber_retry::{self, Next, WEBHOOK_MAX_ATTEMPTS};
use crate::security::sha256_hex;
use crate::storage::locks::try_job;
use crate::storage::subscriber_deliveries::{self, Claimed, Outcome, SentBatch, StoredBatch, Unit};
use crate::storage::subscriber_notices::{self, EMAIL, WEBHOOK};

pub struct SubscriberDispatchConfig {
    pub tick_interval: Duration,
    /// How long a claimed unit is held without a renewal.
    pub lease: Duration,
    /// How often a held unit's lease is renewed; well inside `lease`.
    pub renew_every: Duration,
    pub email_workers: usize,
    pub webhook_workers: usize,
    /// Waits before a batch that got no clear answer is sent again in
    /// process, same key and bytes, before it is stored for later.
    pub resend_waits: Vec<Duration>,
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
            resend_waits: vec![Duration::from_secs(2), Duration::from_secs(5)],
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
        match subscriber_deliveries::oldest_due(&self.pool).await {
            Ok(channels) => {
                for (channel, waited) in channels {
                    metrics::gauge!(metric_names::SUBSCRIBER_OLDEST_DUE, "channel" => channel)
                        .set(waited);
                }
            }
            Err(err) => tracing::warn!(error = %err, "subscriber_dispatch: backlog read failed"),
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
        let Some(unit) =
            subscriber_deliveries::claim(&self.pool, channel, max, token, self.cfg.lease).await?
        else {
            return Ok(false);
        };
        let notice_id = unit.notice.id;
        let tried_before: HashMap<Uuid, i32> =
            unit.deliveries.iter().map(|d| (d.id, d.attempts)).collect();
        let (outcomes, request) = if unit.notice.expired {
            settle_all(unit, "expired", "still unsent a day after the notice")
        } else if !unit.notice.deliverable {
            settle_all(
                unit,
                "skipped",
                "the page or the outage no longer calls for it",
            )
        } else if channel == EMAIL {
            self.send_mail(token, unit).await
        } else {
            (self.send_webhook(token, unit).await, None)
        };
        log_failures(notice_id, channel, &outcomes, &tried_before);
        let batch = request.as_ref().map(|r| SentBatch {
            key: r.batch.key(),
            notice_id,
            request: r.batch.body(),
            delivery_ids: &r.delivery_ids,
        });
        let statuses = subscriber_deliveries::record(&self.pool, token, &outcomes, batch).await?;
        let mut settled: HashMap<String, u64> = HashMap::new();
        for status in statuses.into_iter().filter(|s| s != "queued") {
            *settled.entry(status).or_default() += 1;
        }
        for (status, count) in settled {
            metrics::counter!(metric_names::SUBSCRIBER_DELIVERIES, "channel" => channel, "status" => status)
                .increment(count);
        }
        Ok(true)
    }

    async fn send_mail(&self, token: Uuid, unit: Unit) -> (Vec<Outcome>, Option<MailRequest>) {
        let Unit {
            notice,
            deliveries,
            stored,
        } = unit;
        let kept = stored.is_some();
        let request = match stored {
            Some(stored) => MailRequest::from(stored),
            None => match self.prepare(notice.payload, &deliveries) {
                Ok(request) => request,
                Err(error) => {
                    let outcomes = deliveries
                        .iter()
                        .map(|d| outcome(d, Next::GiveUp(error.clone()), None))
                        .collect();
                    return (outcomes, None);
                }
            },
        };
        let sent = self.send_batch(token, request.batch.clone()).await;
        let Some((answer, maybe_taken)) = sent else {
            return (Vec::new(), None);
        };
        let now = Utc::now();
        let (rows, nexts, keep): (Vec<&Claimed>, Vec<Next>, bool) = match answer {
            // Results go by position in the request as it was built. A
            // delivery removed since has no row left to write. An answered
            // request is settled: sent again under its key it only brings
            // back the same answer, so a mail it left open is built anew.
            Ok(results) => {
                let held: HashMap<Uuid, &Claimed> = deliveries.iter().map(|d| (d.id, d)).collect();
                let mut results = results.into_iter();
                let (rows, nexts): (Vec<&Claimed>, Vec<Next>) = request
                    .delivery_ids
                    .iter()
                    .filter_map(|id| {
                        let result = results.next().unwrap_or_else(|| {
                            Err(EmailError::Transport("no answer for this mail".into()))
                        });
                        let d = *held.get(id)?;
                        let next = match result {
                            Ok(_) => Next::Sent,
                            Err(err) => subscriber_retry::after_mail(&err, d.attempts, now),
                        };
                        Some((d, next))
                    })
                    .unzip();
                (rows, nexts, false)
            }
            // A request the provider may have taken, on this try or any
            // before it, is kept until every delivery in it is settled, so
            // it only ever goes out again as is. One refused as a whole was
            // never taken, so it is let go and built again once fixed.
            Err(failure) => {
                let refused = matches!(failure.error, EmailError::Rejected(_));
                if refused {
                    tracing::error!(
                        notice = %notice.id,
                        error = %failure.error,
                        "subscriber_dispatch: the provider refused a whole batch; it waits until the request is fixed"
                    );
                }
                let nexts = deliveries
                    .iter()
                    .map(|d| subscriber_retry::after_batch(&failure.error, d.attempts, now))
                    .collect();
                (
                    deliveries.iter().collect(),
                    nexts,
                    !refused && (kept || maybe_taken),
                )
            }
        };
        let key = keep.then(|| request.batch.key());
        let outcomes = rows
            .into_iter()
            .zip(nexts)
            .map(|(d, next)| outcome(d, next, key))
            .collect();
        (outcomes, Some(request))
    }

    fn prepare(
        &self,
        payload: serde_json::Value,
        deliveries: &[Claimed],
    ) -> Result<MailRequest, String> {
        let payload: NoticePayload =
            serde_json::from_value(payload).map_err(|e| format!("unreadable notice: {e}"))?;
        let from = EmailAddress::new(self.cfg.from_address.clone(), self.cfg.from_name.clone());
        let emails = deliveries
            .iter()
            .map(|d| {
                let unsubscribe =
                    payload.unsubscribe_url(&self.cfg.unsubscribe_secret, d.subscriber_id);
                payload.email(from.clone(), &d.target, unsubscribe)
            })
            .collect();
        let batch = self
            .email
            .prepare_batch(emails, batch_key(deliveries))
            .map_err(|e| e.to_string())?;
        Ok(MailRequest {
            batch,
            delivery_ids: deliveries.iter().map(|d| d.id).collect(),
        })
    }

    /// Sends a batch, and sends it again under the same key and bytes while
    /// the provider may have taken it without saying so. `true` alongside
    /// the last answer when any try left that open. `None` once the unit is
    /// no longer this claim's.
    async fn send_batch(
        &self,
        token: Uuid,
        mut batch: PreparedBatch,
    ) -> Option<(BatchResult, bool)> {
        let mut maybe_taken = false;
        let mut waits = self.cfg.resend_waits.iter();
        loop {
            let answer = self.held(token, self.email.send_prepared(batch)).await?;
            match answer {
                Err(failure) if subscriber_retry::outcome_unknown(&failure.error) => {
                    maybe_taken = true;
                    let Some(wait) = waits.next() else {
                        return Some((Err(failure), true));
                    };
                    batch = failure.batch;
                    sleep(*wait).await;
                }
                answered => return Some((answered, maybe_taken)),
            }
        }
    }

    /// A webhook unit is one delivery.
    async fn send_webhook(&self, token: Uuid, unit: Unit) -> Vec<Outcome> {
        let [d] = unit.deliveries.as_slice() else {
            let error = "a webhook unit holds one delivery".to_owned();
            return unit
                .deliveries
                .iter()
                .map(|d| outcome(d, Next::GiveUp(error.clone()), None))
                .collect();
        };
        let payload: NoticePayload = match serde_json::from_value(unit.notice.payload) {
            Ok(payload) => payload,
            Err(err) => {
                return vec![outcome(
                    d,
                    Next::GiveUp(format!("unreadable notice: {err}")),
                    None,
                )];
            }
        };
        let unsubscribe = payload.unsubscribe_url(&self.cfg.unsubscribe_secret, d.subscriber_id);
        let Some(body) = payload.webhook(&unsubscribe) else {
            return vec![settle(d, "skipped", "not sent to webhooks")];
        };
        let url = match url::Url::parse(&d.target) {
            Ok(url) => url,
            Err(err) => {
                return vec![outcome(
                    d,
                    Next::GiveUp(format!("bad webhook url: {err}")),
                    None,
                )];
            }
        };
        let body = match serde_json::to_vec(&body) {
            Ok(body) => body,
            Err(err) => {
                return vec![outcome(
                    d,
                    Next::GiveUp(format!("webhook body: {err}")),
                    None,
                )];
            }
        };
        let answer = self
            .held(
                token,
                self.post_webhook(&url, body, d.signing_secret.as_deref()),
            )
            .await;
        match answer {
            None => Vec::new(),
            Some(answer) => vec![webhook_outcome(
                d,
                subscriber_retry::after_webhook(&answer, d.attempts),
            )],
        }
    }

    /// Runs one try while renewing the unit's lease, so the lease only runs
    /// out when this worker is gone. `None` when the rows are no longer this
    /// claim's, found before the try starts or while it runs: they belong to
    /// another sender, and this one sends nothing more. A try already under
    /// way still finishes, so what it has to record about the provider, such
    /// as a rate limit, is not cut off; its answer is dropped.
    async fn held<T>(&self, token: Uuid, work: impl Future<Output = T>) -> Option<T> {
        if !self.owns(token).await {
            return None;
        }
        let lost = async {
            let every = self.cfg.renew_every;
            let mut ticks = interval_at(Instant::now() + every, every);
            loop {
                ticks.tick().await;
                match subscriber_deliveries::renew(&self.pool, token, self.cfg.lease).await {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(err) => {
                        tracing::warn!(error = %err, "subscriber_dispatch: lease renewal failed");
                    }
                }
            }
        };
        tokio::pin!(work);
        tokio::select! {
            out = &mut work => Some(out),
            () = lost => {
                work.await;
                None
            }
        }
    }

    /// Whether the unit is still this claim's, renewing its lease if so. A
    /// check that fails counts as still owned, as a failed renewal does: the
    /// lease was renewed at most one renewal ago and outlasts it.
    async fn owns(&self, token: Uuid) -> bool {
        match subscriber_deliveries::renew(&self.pool, token, self.cfg.lease).await {
            Ok(owned) => owned,
            Err(err) => {
                tracing::warn!(error = %err, "subscriber_dispatch: lease renewal failed");
                true
            }
        }
    }

    /// POST a JSON payload to a subscriber webhook URL through the SSRF-guarded
    /// outbound client (the connector blocks internal/loopback targets even for
    /// a verified URL). Signed with the subscriber's per-row secret so the
    /// receiver can verify authenticity.
    async fn post_webhook(
        &self,
        url: &url::Url,
        body: Vec<u8>,
        signing_secret: Option<&str>,
    ) -> Result<PostAnswer, String> {
        let mut headers = std::collections::BTreeMap::new();
        if let Some(secret) = signing_secret {
            let ts = chrono::Utc::now().timestamp();
            headers.insert("X-Uptimepage-Timestamp".to_string(), ts.to_string());
            headers.insert(
                "X-Uptimepage-Signature".to_string(),
                crate::security::mac::webhook_signature(secret, ts, &body),
            );
        }
        post_json_for_answer(&self.http, url, body, &headers)
            .await
            .map_err(|e| format!("webhook post: {e}"))
    }
}

/// A mail unit as it goes out: the request, and position by position the
/// delivery each of its mails is for.
struct MailRequest {
    batch: PreparedBatch,
    delivery_ids: Vec<Uuid>,
}

impl From<StoredBatch> for MailRequest {
    fn from(stored: StoredBatch) -> Self {
        Self {
            batch: PreparedBatch::from_parts(
                stored.key,
                stored.request.into(),
                stored.delivery_ids.len(),
            ),
            delivery_ids: stored.delivery_ids,
        }
    }
}

/// The same deliveries make the same key, so a batch built again after its
/// sender stopped mid-request is the same request to the provider. Their
/// attempts are part of it: once an answered batch left a mail unsent, that
/// mail's next request is a new one, not a replay of the cached answer.
fn batch_key(deliveries: &[Claimed]) -> String {
    let mut parts: Vec<String> = deliveries
        .iter()
        .map(|d| format!("{}:{}", d.id, d.attempts))
        .collect();
    parts.sort();
    format!("notice-batch:{}", sha256_hex(&parts.join(",")))
}

/// `stored_key` is the stored batch a delivery going back to the queue waits
/// for, if any.
fn outcome(d: &Claimed, next: Next, stored_key: Option<&str>) -> Outcome {
    let (status, error, attempts, retry_in, batch_key) = match next {
        Next::Sent => ("sent", None, d.attempts, Duration::ZERO, None),
        Next::GiveUp(error) => ("failed", Some(error), d.attempts + 1, Duration::ZERO, None),
        Next::Retry {
            error,
            counts,
            after,
            ..
        } => {
            let attempts = d.attempts + i32::from(counts);
            let key = stored_key.map(str::to_owned);
            ("queued", Some(error), attempts, after, key)
        }
    };
    Outcome {
        id: d.id,
        status,
        error,
        attempts,
        retry_in,
        batch_key,
    }
}

/// Only a webhook delivery runs out of attempts.
fn webhook_outcome(d: &Claimed, next: Next) -> Outcome {
    match next {
        Next::Retry {
            error,
            counts: true,
            ..
        } if d.attempts + 1 >= WEBHOOK_MAX_ATTEMPTS => outcome(d, Next::GiveUp(error), None),
        next => outcome(d, next, None),
    }
}

fn settle(d: &Claimed, status: &'static str, why: &str) -> Outcome {
    Outcome {
        id: d.id,
        status,
        error: Some(why.to_owned()),
        attempts: d.attempts,
        retry_in: Duration::ZERO,
        batch_key: None,
    }
}

/// Settles every delivery of a unit unsent. A stored batch goes with them.
fn settle_all(unit: Unit, status: &'static str, why: &str) -> (Vec<Outcome>, Option<MailRequest>) {
    let outcomes = unit
        .deliveries
        .iter()
        .map(|d| settle(d, status, why))
        .collect();
    let request = unit.stored.map(MailRequest::from);
    (outcomes, request)
}

/// One line per unit, not per delivery: a batch shares its answer. A retry
/// that used an attempt is a warning; one only waiting on pacing is not.
fn log_failures(
    notice: Uuid,
    channel: &str,
    outcomes: &[Outcome],
    tried_before: &HashMap<Uuid, i32>,
) {
    let failed = outcomes.iter().filter(|o| o.status == "failed").count();
    let (tried, waiting): (Vec<&Outcome>, Vec<&Outcome>) = outcomes
        .iter()
        .filter(|o| o.status == "queued")
        .partition(|o| tried_before.get(&o.id).is_some_and(|&n| o.attempts > n));
    let error = outcomes
        .iter()
        .find(|o| o.status == "failed" || o.status == "queued")
        .and_then(|o| o.error.as_deref());
    let (tried, waiting) = (tried.len(), waiting.len());
    if failed > 0 || tried > 0 {
        tracing::warn!(%notice, channel, failed, tried, waiting, error, "subscriber deliveries failed");
    } else if waiting > 0 {
        tracing::debug!(%notice, channel, waiting, error, "subscriber deliveries wait to be sent");
    }
}
