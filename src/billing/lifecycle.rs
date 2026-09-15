//! The subscription state machine: what a provider event or the clock does to
//! an account.
//!
//! Three rules shape every transition:
//!
//! - **Up now, down later.** A more generous plan applies the moment the
//!   provider confirms it; a smaller one, or a cancel, waits for the paid
//!   period to end. The customer keeps what they paid for.
//! - **A failed payment never cuts service on day zero.** The account goes
//!   `past_due` and keeps its plan for [`GRACE_DAYS`], with reminders on
//!   [`REMINDER_DAYS`]; the first failure fixes the deadline. Only when it
//!   passes unpaid does the plan fall to the fallback.
//! - **Events arrive in any order, and more than once.** Each is claimed by
//!   id before it is applied, and one older than the last applied observation
//!   is dropped rather than rewinding the account.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};

use super::mail::Mailer;
use super::provider::SubscriptionStatus as Remote;
use super::provider::{
    BillingProvider, ChangeTiming, EventKind, ProviderEvent, SubscriptionSnapshot,
};
use super::{Actor, PlanRequest, set_plan_tx};
use crate::api::error::codes;
use crate::domain::{AccountId, BillingStatus, Interval, Landing, Subscription};
use crate::email::EmailTemplate;
use crate::error::{AppError, Result};
use crate::observability::metrics::names;
use crate::quotas::{QuotaService, holds, reconcile_after_change};
use crate::storage::billing_events as ledger;
use crate::storage::subscriptions::{self as store};

pub const GRACE_DAYS: i64 = 14;
/// Days into the grace window a reminder goes out; the first at the failure.
pub const REMINDER_DAYS: [i64; 4] = [0, 3, 7, 12];

/// Accounts one sweep tick looks at. Far above any real backlog; the rest
/// is picked up next tick.
const SWEEP_BATCH: i64 = 500;
/// How long a provider event id is remembered. Paddle redelivers for three
/// days; a month also covers a destination that was down for a while and
/// leaves a trail to read when a customer asks what happened.
pub const EVENT_RETENTION_DAYS: i64 = 30;

pub struct Billing {
    pub provider: Arc<dyn BillingProvider>,
    pub mailer: Mailer,
    /// The after-effects the receiver answered ahead of, so a shutdown can
    /// wait for them and a burst of deliveries settles a few at a time.
    settling: TaskTracker,
    settle_slots: Arc<Semaphore>,
    /// One account's changes, webhooks and sweeps take turns, so a change
    /// made of several provider requests is never read in between. Held in
    /// this process only; two processes can still interleave, and a move
    /// read in between then lands on the period end the row already paid
    /// for, not the restarted one the snapshot shows.
    turns: Turns,
}

type Turns = Arc<Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>>;
/// An account's turn, held until dropped.
type Turn = tokio::sync::OwnedMutexGuard<()>;

impl Billing {
    pub fn new(provider: Arc<dyn BillingProvider>, mailer: Mailer) -> Self {
        Self {
            provider,
            mailer,
            settling: TaskTracker::new(),
            settle_slots: Arc::new(Semaphore::new(SETTLE_SLOTS)),
            turns: Arc::default(),
        }
    }

    /// Entries nobody holds or waits on (a holder or waiter keeps its own
    /// handle) go the next time any account's is taken.
    fn slot(&self, account: AccountId) -> Arc<tokio::sync::Mutex<()>> {
        let mut turns = self.turns.lock().unwrap_or_else(|p| p.into_inner());
        turns.retain(|_, m| Arc::strong_count(m) > 1);
        Arc::clone(turns.entry(account).or_default())
    }

    async fn turn(&self, account: AccountId) -> Turn {
        self.slot(account).lock_owned().await
    }

    /// The turn only if nobody has it: the sweep passes a busy account by
    /// and finds it next tick.
    fn try_turn(&self, account: AccountId) -> Option<Turn> {
        self.slot(account).try_lock_owned().ok()
    }

    /// The turn, or a refusal once `patience` is up.
    async fn turn_within(&self, account: AccountId, patience: StdDuration) -> Result<Turn> {
        tokio::time::timeout(patience, self.turn(account))
            .await
            .map_err(|_| {
                AppError::conflict(
                    codes::SUBSCRIPTION_STATE,
                    "another change to the subscription is still being applied; try again in a moment",
                )
            })
    }

    /// Waits for every after-effect the receiver has answered ahead of.
    pub async fn drain(&self) {
        self.settling.close();
        self.settling.wait().await;
    }
}

/// What became of one delivery, for the receiver's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    /// Already acted on under this id.
    Duplicate,
    /// Older than what the account already reflects.
    Stale,
    /// Names no account we know.
    Unmatched,
    /// Carried a subscription that is not the account's live one.
    Foreign,
    /// The clock owed the account nothing.
    Unchanged,
}

/// The provider counts an answer later than five seconds as none, so a
/// delivery gets this long to be committed; past it the delivery is refused
/// and comes again, landing as a duplicate if the commit had gone through.
pub const ACK_BUDGET: StdDuration = StdDuration::from_secs(3);
/// After-effects settled at once, so a replayed batch of deliveries does not
/// hit the provider and the mailer with everything at the same time.
const SETTLE_SLOTS: usize = 4;
/// How long a change waits for the account's turn: one change ahead of it,
/// so a client that resends cannot line up changes that outlive it and hold
/// the account against its webhooks.
const CHANGE_PATIENCE: StdDuration = StdDuration::from_secs(5);

/// Side effects the transaction defers until it has committed.
#[derive(Default)]
struct Effects {
    plan_moved: bool,
    mails: Vec<Mail>,
    /// Left alone, each of these keeps charging.
    cancel_at_provider: Vec<String>,
}

impl Effects {
    fn is_empty(&self) -> bool {
        !self.plan_moved && self.mails.is_empty() && self.cancel_at_provider.is_empty()
    }
}

enum Mail {
    Ready(Box<EmailTemplate>),
    /// Needs the held counts, which exist only after the reconcile.
    Landed {
        plan_name: String,
        landing: Landing,
    },
}

impl Effects {
    fn mail(&mut self, template: EmailTemplate) {
        self.mails.push(Mail::Ready(Box::new(template)));
    }
}

/// What a committed change leaves to do outside its transaction.
struct Aftermath {
    sub: Subscription,
    fx: Effects,
}

enum Input {
    Event(Box<ProviderEvent>, Option<Box<SubscriptionSnapshot>>),
    /// The provider's view, looked up.
    Snapshot(Box<SubscriptionSnapshot>),
    /// The provider's answer to a change the app asked for.
    Answer(Box<SubscriptionSnapshot>),
    Clock(DateTime<Utc>),
}

impl Billing {
    /// Applies one verified delivery and everything that follows from it.
    /// The returned outcome is informational; every `Ok` is an
    /// acknowledgement, and an `Err` asks the provider to deliver again.
    pub async fn apply_event(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        event: ProviderEvent,
    ) -> Result<Outcome> {
        let (outcome, after) = self.take_in(pool, event).await?;
        if let Some(after) = after {
            self.settle(pool, quotas, &after.sub, after.fx).await;
        }
        Ok(outcome)
    }

    /// The receiver's path: the delivery is acknowledged once its change is
    /// committed, within [`ACK_BUDGET`], and what follows (the reconcile, the
    /// mails, a cancel at the provider) runs on its own, since the provider
    /// counts a slow answer as no answer and delivers again. The whole
    /// delivery is one detached task: a budget that runs out, or a sender
    /// that hangs up, leaves the commit and its after-effects to finish,
    /// where a dropped future could have committed and lost them, with the
    /// redelivery then answered as a duplicate.
    pub async fn receive(
        self: &Arc<Self>,
        pool: &PgPool,
        quotas: &Arc<QuotaService>,
        event: ProviderEvent,
    ) -> Result<Outcome> {
        let event_id = event.event_id.clone();
        let billing = Arc::clone(self);
        let pool = pool.clone();
        let quotas = Arc::clone(quotas);
        let (answer, answered) = tokio::sync::oneshot::channel();
        self.settling.spawn(async move {
            let after = match billing.take_in(&pool, event).await {
                Ok((outcome, after)) => {
                    let _ = answer.send(Ok(outcome));
                    after
                }
                Err(err) => {
                    if let Err(Err(err)) = answer.send(Err(err)) {
                        tracing::warn!(error = %err, "billing: a delivery answered as late then failed");
                    }
                    None
                }
            };
            if let Some(after) = after {
                let _slot = billing.settle_slots.acquire().await;
                billing.settle(&pool, &quotas, &after.sub, after.fx).await;
            }
        });
        match tokio::time::timeout(ACK_BUDGET, answered).await {
            Ok(Ok(taken)) => taken,
            Ok(Err(_)) => Err(AppError::Other(anyhow::anyhow!(
                "billing: the delivery's task ended without an answer"
            ))),
            Err(_) => {
                tracing::warn!(
                    event_id,
                    "billing: delivery not committed within the budget"
                );
                Err(AppError::service_unavailable(
                    codes::BILLING_PROVIDER_UNREACHABLE,
                    "the delivery could not be committed in time",
                ))
            }
        }
    }

    async fn take_in(
        &self,
        pool: &PgPool,
        event: ProviderEvent,
    ) -> Result<(Outcome, Option<Aftermath>)> {
        let provider = self.provider.name();
        // A redelivery of an event already applied is answered without the
        // account's turn or a provider call; `claim_event` still decides
        // for two deliveries of one event racing each other.
        if store::event_seen(pool, provider, &event.event_id).await? {
            return Ok((Outcome::Duplicate, None));
        }
        let Some(account) = resolve_account(pool, provider, &event).await? else {
            self.acknowledge_unmatched(pool, &event).await?;
            return Ok((unowned(&event.kind), None));
        };
        // A first payment can land before the subscription event it belongs
        // to. The provider is asked once, before any lock is held.
        let fetched = self.fetch_for_paid(pool, account, &event).await?;
        self.commit(
            pool,
            account,
            Input::Event(Box::new(event), fetched.map(Box::new)),
        )
        .await
    }

    /// Applies a snapshot the provider returned to a lookup, so the caller
    /// sees the provider's view without waiting for a webhook.
    pub(super) async fn apply_snapshot(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
        snapshot: SubscriptionSnapshot,
    ) -> Result<Outcome> {
        self.apply(pool, quotas, account, Input::Snapshot(Box::new(snapshot)))
            .await
    }

    /// Asks the provider for a change and applies its answer in the
    /// account's turn. `call` gets the row as it is in that turn and the
    /// provider, decides, and answers with the snapshot to apply, or none
    /// when there is nothing to do. The webhooks a change sets off can land
    /// before the call returns, and a change made in more than one request
    /// is wrong in between; they wait for the turn, and the in-between ones
    /// then read as older than the answer and are dropped, while the last
    /// repeats it. Turn, call and commit run as one detached task, so a
    /// request dropped mid-change still finishes every step at the provider
    /// with the account held throughout. A turn not had within
    /// [`CHANGE_PATIENCE`] is a change refused, not queued. An answer the
    /// row would not take (another process moved it on first) is logged,
    /// since the provider call was still made.
    pub async fn change<Fut>(
        self: &Arc<Self>,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
        call: impl FnOnce(Subscription, Arc<dyn BillingProvider>) -> Fut + Send + 'static,
    ) -> Result<()>
    where
        Fut: Future<Output = Result<Option<SubscriptionSnapshot>>> + Send,
    {
        let this = Arc::clone(self);
        let pool = pool.clone();
        let quotas = quotas.clone();
        self.settling
            .spawn(async move {
                let turn = this.turn_within(account, CHANGE_PATIENCE).await?;
                let sub = this.subscription(&pool, account).await?;
                let Some(snapshot) = call(sub, Arc::clone(&this.provider)).await? else {
                    return Ok(());
                };
                let (outcome, after) = this
                    .commit_in_turn(&pool, account, Input::Answer(Box::new(snapshot)))
                    .await?;
                drop(turn);
                if outcome != Outcome::Applied {
                    tracing::warn!(account = %account, ?outcome, "billing: the provider's answer to a change was not taken");
                }
                if let Some(after) = after {
                    this.settle(&pool, &quotas, &after.sub, after.fx).await;
                }
                Ok(())
            })
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("billing: provider change task: {e}")))?
    }

    /// Applies what the clock owes: scheduled changes that are due, grace
    /// windows that ran out, reminders that fell due. Returns how many
    /// accounts changed.
    pub async fn sweep(&self, pool: &PgPool, quotas: &QuotaService) -> Result<u64> {
        let now = Utc::now();
        let mut changed = 0;
        for account in store::due_for_sweep(pool, now, SWEEP_BATCH).await? {
            match self.apply(pool, quotas, account, Input::Clock(now)).await {
                Ok(Outcome::Applied) => changed += 1,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(account = %account, error = %err, "billing sweep: account skipped")
                }
            }
        }
        let pruned = store::prune_events(pool, now - Duration::days(EVENT_RETENTION_DAYS)).await?;
        if pruned > 0 {
            tracing::debug!(pruned, "billing sweep: old provider events dropped");
        }
        publish_status_gauge(pool).await?;
        Ok(changed)
    }

    async fn apply(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
        input: Input,
    ) -> Result<Outcome> {
        let (outcome, after) = self.commit(pool, account, input).await?;
        if let Some(after) = after {
            self.settle(pool, quotas, &after.sub, after.fx).await;
        }
        Ok(outcome)
    }

    async fn commit(
        &self,
        pool: &PgPool,
        account: AccountId,
        input: Input,
    ) -> Result<(Outcome, Option<Aftermath>)> {
        let _turn = match &input {
            Input::Clock(_) => match self.try_turn(account) {
                Some(turn) => turn,
                None => {
                    tracing::debug!(account = %account, "billing sweep: account busy, next tick");
                    return Ok((Outcome::Unchanged, None));
                }
            },
            Input::Snapshot(_) | Input::Answer(_) => {
                self.turn_within(account, CHANGE_PATIENCE).await?
            }
            Input::Event(..) => self.turn(account).await,
        };
        self.commit_in_turn(pool, account, input).await
    }

    async fn commit_in_turn(
        &self,
        pool: &PgPool,
        account: AccountId,
        input: Input,
    ) -> Result<(Outcome, Option<Aftermath>)> {
        let now = Utc::now();
        let mut tx = pool.begin().await.context("billing apply: begin")?;
        if let Input::Event(event, _) = &input
            && !store::claim_event(
                &mut tx,
                self.provider.name(),
                &event.event_id,
                &event.event_type,
                event.occurred_at,
            )
            .await?
        {
            return Ok((Outcome::Duplicate, None));
        }
        let Some(mut sub) = store::lock(&mut tx, account).await? else {
            tx.commit().await.context("billing apply: commit")?;
            let outcome = match &input {
                Input::Event(event, _) => unowned(&event.kind),
                _ => Outcome::Unmatched,
            };
            return Ok((outcome, None));
        };
        let before = sub.clone();
        let mut fx = Effects::default();

        let outcome = match input {
            Input::Event(event, fetched) => {
                self.on_event(&mut tx, &mut sub, *event, fetched.map(|f| *f), now, &mut fx)
                    .await?
            }
            Input::Snapshot(snapshot) => {
                self.on_snapshot(&mut tx, &mut sub, *snapshot, false, now, &mut fx)
                    .await?
            }
            Input::Answer(snapshot) => {
                self.on_snapshot(&mut tx, &mut sub, *snapshot, true, now, &mut fx)
                    .await?
            }
            Input::Clock(now) => {
                self.on_clock(&mut tx, &mut sub, now, &mut fx).await?;
                if sub == before {
                    Outcome::Unchanged
                } else {
                    Outcome::Applied
                }
            }
        };

        if sub != before {
            store::write(&mut tx, &sub).await?;
        }
        tx.commit().await.context("billing apply: commit")?;
        let after = (!fx.is_empty()).then_some(Aftermath { sub, fx });
        Ok((outcome, after))
    }

    async fn on_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        event: ProviderEvent,
        fetched: Option<SubscriptionSnapshot>,
        now: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<Outcome> {
        if matches!(event.kind, EventKind::Other) {
            return Ok(Outcome::Applied);
        }
        // A stranger's event is answered whatever its age.
        if !binds(sub, event.subscription_ref.as_deref()) {
            let live = matches!(&event.kind, EventKind::Subscription(s) if s.status.is_live());
            self.note_foreign(
                tx,
                sub,
                &event.event_type,
                event.subscription_ref.as_deref(),
                live,
                fx,
            )
            .await?;
            return Ok(Outcome::Foreign);
        }
        // Payments never move the snapshot watermark, so they are also
        // ordered among themselves.
        let watermark = match event.kind {
            EventKind::Paid | EventKind::PaymentFailed => sub.synced_at.max(sub.payment_synced_at),
            _ => sub.synced_at,
        };
        if watermark.is_some_and(|at| event.occurred_at < at) {
            return Ok(Outcome::Stale);
        }
        match event.kind {
            EventKind::Paid => {
                sub.payment_synced_at = Some(event.occurred_at);
                match (sub.status, fetched) {
                    (BillingStatus::PastDue, _) => self.recover(tx, sub, fx).await?,
                    (BillingStatus::None | BillingStatus::Canceled, Some(snapshot)) => {
                        return self.on_snapshot(tx, sub, snapshot, false, now, fx).await;
                    }
                    _ => {}
                }
            }
            EventKind::PaymentFailed => {
                sub.payment_synced_at = Some(event.occurred_at);
                if sub.status == BillingStatus::Active {
                    self.start_grace(tx, sub, now, fx).await?;
                }
            }
            EventKind::Subscription(snapshot) => {
                return self.on_snapshot(tx, sub, snapshot, false, now, fx).await;
            }
            EventKind::Other => {}
        }
        Ok(Outcome::Applied)
    }

    /// `asked` is the provider answering a change the app made: an ending it
    /// reports then is the app's own, not its retry schedule giving up.
    async fn on_snapshot(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        snap: SubscriptionSnapshot,
        asked: bool,
        now: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<Outcome> {
        if !binds(sub, Some(&snap.subscription_ref)) {
            self.note_foreign(
                tx,
                sub,
                "snapshot",
                Some(&snap.subscription_ref),
                snap.status.is_live(),
                fx,
            )
            .await?;
            return Ok(Outcome::Foreign);
        }
        if sub.synced_at.is_some_and(|at| snap.taken_at < at) {
            return Ok(Outcome::Stale);
        }
        sub.synced_at = Some(snap.taken_at);
        let live = matches!(sub.status, BillingStatus::Active | BillingStatus::PastDue);
        // A cancel whose date has come is over, whatever status the provider
        // still shows until its own event lands.
        let over = snap.cancel_at.is_some_and(|at| at <= now);
        let going_live = matches!(snap.status, Remote::Active | Remote::Trialing) && !over;
        if live || going_live {
            sub.provider = Some(self.provider.name().to_owned());
            sub.customer_ref = Some(snap.customer_ref.clone());
            sub.subscription_ref = Some(snap.subscription_ref.clone());
        }

        // A failed or recovered payment arrives twice, as an event and as a
        // snapshot; the newer of the two has the last word.
        let after_last_payment = sub.payment_synced_at.is_none_or(|at| at <= snap.taken_at);
        match snap.status {
            Remote::Active | Remote::Trialing if going_live => {
                match sub.status {
                    BillingStatus::PastDue if after_last_payment => {
                        self.recover(tx, sub, fx).await?;
                    }
                    BillingStatus::PastDue => {}
                    _ => sub.status = BillingStatus::Active,
                }
                let price = self.settle_plan(tx, sub, &snap, now, fx).await?;
                sub.current_period_end = snap.period_end;
                // A booked move reports its price ahead of time; the cadence
                // shown is the one being paid until it lands.
                if sub.plan_id == price.plan_id {
                    sub.interval = Interval::parse(&price.interval);
                }
            }
            Remote::PastDue if !over => {
                if live {
                    sub.current_period_end = snap.period_end;
                    self.settle_cancel(tx, sub, &snap, fx).await?;
                }
                if sub.status == BillingStatus::Active && after_last_payment {
                    self.start_grace(tx, sub, now, fx).await?;
                }
            }
            // A cancel that outran its own activation, or a stray purchase
            // ending, has no plan of its own to remove. An unpaid subscription
            // the provider ends on its own, with no cancel booked and none
            // asked for here, ends for the payment that never came.
            _ => {
                if live {
                    let booked = sub.cancel_at.is_some() || snap.cancel_at.is_some();
                    let landing = match snap.status {
                        _ if sub.status == BillingStatus::PastDue && !booked && !asked => {
                            Landing::Unpaid
                        }
                        Remote::Paused => Landing::Paused,
                        _ => Landing::Canceled,
                    };
                    self.end_service(tx, sub, landing, fx).await?;
                }
            }
        }
        Ok(Outcome::Applied)
    }

    /// None or several is refused so the provider retries once the rows are
    /// right.
    async fn plan_for(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        snap: &SubscriptionSnapshot,
    ) -> Result<store::PlanPrice> {
        let mut prices =
            store::plans_for_prices(&mut **tx, self.provider.name(), &snap.price_refs).await?;
        match prices.len() {
            1 => Ok(prices.remove(0)),
            0 => Err(AppError::Other(anyhow::anyhow!(
                "subscription {} carries no price in plan_prices: {:?}",
                snap.subscription_ref,
                snap.price_refs
            ))),
            _ => Err(AppError::Other(anyhow::anyhow!(
                "subscription {} carries more than one plan price: {:?}",
                snap.subscription_ref,
                snap.price_refs
            ))),
        }
    }

    async fn on_clock(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        now: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<()> {
        if let Some(at) = sub.cancel_at
            && at <= now
        {
            // A booked cancel reaching its date. end_service takes the account
            // to `canceled` and holds the excess; the provider's own
            // `subscription.canceled` sets the same state when it arrives,
            // whichever lands first.
            self.end_service(tx, sub, Landing::Canceled, fx).await?;
        } else if let (Some(pending), Some(at)) = (sub.pending_plan_id.clone(), sub.plan_change_at)
            && at <= now
        {
            self.move_now(tx, sub, &pending, "scheduled change", Actor::System, fx)
                .await?;
            // The provider has billed this cadence since the move was booked.
            sub.interval = sub.pending_interval.or(sub.interval);
            sub.clear_pending();
            fx.mails.push(Mail::Landed {
                plan_name: plan_name(&mut **tx, &pending).await?,
                landing: Landing::Scheduled,
            });
        }
        if sub.status == BillingStatus::PastDue
            && let Some(grace_until) = sub.grace_until
        {
            if grace_until <= now {
                ledger::record_tx(tx, sub.account, ledger::GRACE_EXPIRED, json!({})).await?;
                self.end_service(tx, sub, Landing::Unpaid, fx).await?;
                // A card retry succeeding months later would charge for a
                // plan the account no longer holds.
                fx.cancel_at_provider.extend(sub.subscription_ref.clone());
            } else if let Some(stage) = reminder_due(grace_until, now, sub.dunning_stage) {
                sub.dunning_stage = stage;
                ledger::record_tx(
                    tx,
                    sub.account,
                    ledger::PAYMENT_REMINDER,
                    json!({ "stage": stage }),
                )
                .await?;
                fx.mail(EmailTemplate::PaymentFailed {
                    plan_name: plan_name(&mut **tx, &sub.plan_id).await?,
                    retry_by: grace_until,
                    fix_url: self.mailer.fix_url(),
                });
            }
        }
        Ok(())
    }

    /// A cancel booked or withdrawn at the provider lands on the row whatever
    /// the payment state; answers whether one stands. A booked move cannot
    /// outlive the subscription. An unpaid account is told the earlier of the
    /// cancel date and its grace deadline, since service ends at the deadline
    /// unless paid.
    async fn settle_cancel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        snap: &SubscriptionSnapshot,
        fx: &mut Effects,
    ) -> Result<bool> {
        let Some(at) = snap.cancel_at else {
            if sub.cancel_at.take().is_some() {
                ledger::record_tx(tx, sub.account, ledger::PENDING_CHANGE_CLEARED, json!({}))
                    .await?;
            }
            return Ok(false);
        };
        if sub.cancel_at != Some(at) {
            sub.cancel_at = Some(at);
            sub.clear_pending();
            let landing = sub.landing_plan().to_owned();
            ledger::record_tx(
                tx,
                sub.account,
                ledger::CANCEL_SCHEDULED,
                json!({ "ends_at": at, "then": landing }),
            )
            .await?;
            let ends_at = match (sub.status, sub.grace_until) {
                (BillingStatus::PastDue, Some(deadline)) => at.min(deadline),
                _ => at,
            };
            fx.mail(EmailTemplate::SubscriptionCanceled {
                plan_name: plan_name(&mut **tx, &landing).await?,
                ends_at,
            });
        }
        Ok(true)
    }

    /// Decides what the provider's plan means for the account's own: now for
    /// a bigger plan, at period end for a smaller one, and a scheduled cancel
    /// over either; answers the price the snapshot names.
    /// Runs before the row takes the snapshot's period end: a smaller plan
    /// lands on the period end the row already paid for, since a change
    /// made in more than one provider request shows a restarted period in
    /// between, in either direction, and the snapshot's own period end is
    /// only trusted once the row's is over. A smaller price put on in the
    /// provider's dashboard with immediate proration lands on the old date
    /// for the same reason; such moves are made through the app.
    async fn settle_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        snap: &SubscriptionSnapshot,
        now: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<store::PlanPrice> {
        let price = self.plan_for(tx, snap).await?;
        let target = price.plan_id.as_str();
        if self.settle_cancel(tx, sub, snap, fx).await? {
            if target != sub.plan_id && !smaller(tx, target, &sub.plan_id).await? {
                self.move_now(tx, sub, target, "provider subscription", self.actor(), fx)
                    .await?;
            }
            return Ok(price);
        }
        if target == sub.plan_id {
            if sub.pending_plan_id.is_some() {
                sub.clear_pending();
                ledger::record_tx(tx, sub.account, ledger::PENDING_CHANGE_CLEARED, json!({}))
                    .await?;
            }
            return Ok(price);
        }
        if !smaller(tx, target, &sub.plan_id).await? {
            self.move_now(tx, sub, target, "provider subscription", self.actor(), fx)
                .await?;
            sub.clear_pending();
            return Ok(price);
        }
        let already = sub.pending_plan_id.as_deref() == Some(target);
        match (already, sub.plan_change_at) {
            (true, Some(at)) if at <= now => {
                self.move_now(tx, sub, target, "scheduled change", self.actor(), fx)
                    .await?;
                sub.clear_pending();
                fx.mails.push(Mail::Landed {
                    plan_name: plan_name(&mut **tx, target).await?,
                    landing: Landing::Scheduled,
                });
            }
            // The same move at the other cadence.
            (true, Some(at)) => {
                let cadence = Interval::parse(&price.interval);
                if sub.pending_interval != cadence {
                    sub.pending_interval = cadence;
                    record_booked(tx, sub.account, &price, at).await?;
                }
            }
            (true, None) => {}
            _ => match snap
                .period_end
                .map(|end| sub.current_period_end.filter(|at| *at > now).unwrap_or(end))
            {
                // No boundary to defer to (a snapshot without a billing period,
                // e.g. a trialing or paused subscription): keep the plan the
                // customer paid for and wait for a snapshot that carries one,
                // rather than cutting service the moment the event lands.
                None => {}
                Some(at) if at <= now => {
                    self.move_now(tx, sub, target, "provider subscription", self.actor(), fx)
                        .await?;
                    sub.clear_pending();
                }
                Some(at) => {
                    self.schedule_downgrade(tx, sub, &price, at, fx).await?;
                }
            },
        }
        Ok(price)
    }

    async fn schedule_downgrade(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        price: &store::PlanPrice,
        at: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<()> {
        let target = price.plan_id.as_str();
        sub.pending_plan_id = Some(target.to_owned());
        sub.plan_change_at = Some(at);
        sub.pending_interval = Interval::parse(&price.interval);
        let brief = store::plan_brief(&mut **tx, target)
            .await?
            .ok_or_else(|| AppError::Other(anyhow::anyhow!("plan {target:?} vanished")))?;
        let counts = pooled_counts(tx, sub.account).await?;
        record_booked(tx, sub.account, price, at).await?;
        fx.mail(EmailTemplate::DowngradeScheduled {
            over_monitors: counts.monitors_over(&brief),
            over_pages: (counts.status_pages - i64::from(brief.max_status_pages)).max(0),
            plan_name: brief.name,
            at,
            keep_url: self.mailer.keep_url(),
        });
        Ok(())
    }

    async fn start_grace(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        now: DateTime<Utc>,
        fx: &mut Effects,
    ) -> Result<()> {
        sub.status = BillingStatus::PastDue;
        let grace_until = now + Duration::days(GRACE_DAYS);
        sub.grace_until = Some(grace_until);
        sub.dunning_stage = 1;
        ledger::record_tx(
            tx,
            sub.account,
            ledger::GRACE_STARTED,
            json!({ "until": grace_until }),
        )
        .await?;
        fx.mail(EmailTemplate::PaymentFailed {
            plan_name: plan_name(&mut **tx, &sub.plan_id).await?,
            retry_by: grace_until,
            fix_url: self.mailer.fix_url(),
        });
        Ok(())
    }

    async fn recover(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        fx: &mut Effects,
    ) -> Result<()> {
        sub.status = BillingStatus::Active;
        sub.clear_grace();
        ledger::record_tx(tx, sub.account, ledger::PAYMENT_RECOVERED, json!({})).await?;
        fx.mail(EmailTemplate::PaymentRecovered {
            plan_name: plan_name(&mut **tx, &sub.plan_id).await?,
        });
        Ok(())
    }

    /// Paid service is over: the account lands on its fallback plan. The
    /// provider's customer stays so a return needs no new details.
    async fn end_service(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        landing: Landing,
        fx: &mut Effects,
    ) -> Result<()> {
        let plan = sub.landing_plan().to_owned();
        if sub.status == BillingStatus::Canceled && sub.plan_id == plan {
            return Ok(());
        }
        sub.status = BillingStatus::Canceled;
        sub.interval = None;
        sub.current_period_end = None;
        sub.cancel_at = None;
        sub.clear_pending();
        sub.clear_grace();
        let moved = sub.plan_id != plan;
        if moved {
            let reason = match landing {
                Landing::Unpaid => "grace expired",
                Landing::Canceled | Landing::Paused | Landing::Scheduled => "subscription ended",
            };
            self.move_now(tx, sub, &plan, reason, Actor::System, fx)
                .await?;
        }
        ledger::record_tx(
            tx,
            sub.account,
            ledger::SUBSCRIPTION_ENDED,
            json!({ "plan": plan, "landing": landing.as_str() }),
        )
        .await?;
        // Already told when the scheduled move landed; the status flip alone
        // is not news.
        if moved {
            fx.mails.push(Mail::Landed {
                plan_name: plan_name(&mut **tx, &plan).await?,
                landing,
            });
        }
        Ok(())
    }

    fn actor(&self) -> Actor {
        Actor::Provider {
            name: self.provider.name(),
        }
    }

    async fn move_now(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &mut Subscription,
        target: &str,
        reason: &str,
        actor: Actor,
        fx: &mut Effects,
    ) -> Result<()> {
        let write = set_plan_tx(
            tx,
            sub.account,
            PlanRequest {
                plan_id: target,
                fallback_plan_id: None,
                reason,
                actor,
            },
        )
        .await?;
        sub.plan_id = write.to;
        sub.fallback_plan_id = write.fallback;
        fx.plan_moved |= write.changed;
        Ok(())
    }

    /// Two checkouts completed back to back, or a purchase made under this
    /// account's id from outside: the extra one can serve nobody, so it is
    /// ended rather than left to charge.
    async fn note_foreign(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sub: &Subscription,
        event_type: &str,
        subscription_ref: Option<&str>,
        live: bool,
        fx: &mut Effects,
    ) -> Result<()> {
        tracing::warn!(
            account = %sub.account,
            event_type,
            subscription_ref,
            live,
            "billing: event for a subscription that is not the account's, ignored"
        );
        if live && let Some(subscription_ref) = subscription_ref {
            fx.cancel_at_provider.push(subscription_ref.to_owned());
        }
        ledger::record_tx(
            tx,
            sub.account,
            ledger::FOREIGN_SUBSCRIPTION_IGNORED,
            json!({ "event_type": event_type, "subscription_ref": subscription_ref }),
        )
        .await
    }

    /// A first payment for an account with no live subscription: the
    /// subscription it created is fetched so the plan can be applied even if
    /// the subscription event is late or lost.
    async fn fetch_for_paid(
        &self,
        pool: &PgPool,
        account: AccountId,
        event: &ProviderEvent,
    ) -> Result<Option<SubscriptionSnapshot>> {
        let (EventKind::Paid, Some(subscription_ref)) = (&event.kind, &event.subscription_ref)
        else {
            return Ok(None);
        };
        let Some(sub) = store::get(pool, account).await? else {
            return Ok(None);
        };
        if !matches!(sub.status, BillingStatus::None | BillingStatus::Canceled) {
            return Ok(None);
        }
        Ok(Some(
            self.provider.fetch_subscription(subscription_ref).await?,
        ))
    }

    /// Claims an event that names no account, so the provider stops
    /// redelivering it.
    async fn acknowledge_unmatched(&self, pool: &PgPool, event: &ProviderEvent) -> Result<()> {
        let mut tx = pool.begin().await.context("billing apply: begin")?;
        let fresh = store::claim_event(
            &mut tx,
            self.provider.name(),
            &event.event_id,
            &event.event_type,
            event.occurred_at,
        )
        .await?;
        tx.commit().await.context("billing apply: commit")?;
        if fresh && !matches!(event.kind, EventKind::Other) {
            tracing::warn!(
                event_id = %event.event_id,
                event_type = %event.event_type,
                "billing: event names no account, acknowledged without effect"
            );
        }
        Ok(())
    }

    /// After the commit: the reconcile that applies the new caps, then the
    /// mails, which can name what the reconcile held.
    async fn settle(&self, pool: &PgPool, quotas: &QuotaService, sub: &Subscription, fx: Effects) {
        if fx.plan_moved
            && let Err(err) = reconcile_after_change(pool, quotas, sub.account, None).await
        {
            tracing::warn!(account = %sub.account, error = %err, "billing: reconcile after plan move");
        }
        for subscription_ref in fx.cancel_at_provider {
            // Detached so an owner's request dropped mid-call (a plan change,
            // a checkout) still ends the subscription; awaited so the caller
            // sees it done. The receiver has already answered by now.
            let provider = Arc::clone(&self.provider);
            let account = sub.account;
            if let Err(err) = tokio::spawn(async move {
                end_at_provider(provider.as_ref(), account, &subscription_ref).await
            })
            .await
            {
                tracing::error!(account = %sub.account, error = %err, "billing: provider cancel task");
            }
        }
        for mail in fx.mails {
            let template = match mail {
                Mail::Ready(template) => *template,
                Mail::Landed { plan_name, landing } => {
                    let (targets, pages) = match holds::list_held(pool, sub.account).await {
                        Ok(held) => held,
                        Err(err) => {
                            tracing::warn!(account = %sub.account, error = %err, "billing: held counts");
                            (Vec::new(), Vec::new())
                        }
                    };
                    EmailTemplate::DowngradeApplied {
                        plan_name,
                        landing,
                        held_monitors: targets.len(),
                        held_pages: pages.len(),
                        keep_url: self.mailer.keep_url(),
                    }
                }
            };
            self.mailer
                .send(pool, sub.account, sub.owner, template)
                .await;
        }
    }
}

/// Ends a subscription that can serve nobody. Anything short of the provider
/// showing it ending is counted, so the page names what to end by hand.
async fn end_at_provider(
    provider: &dyn BillingProvider,
    account: AccountId,
    subscription_ref: &str,
) {
    let refused = match cancel_despite_refusal(provider, subscription_ref, ChangeTiming::Now).await
    {
        Ok(snapshot) if snapshot.is_ending() => return,
        Ok(snapshot) => format!("answered but left it {:?}", snapshot.status),
        Err(err) => err.to_string(),
    };
    metrics::counter!(names::BILLING_PROVIDER_CANCEL_FAILED).increment(1);
    tracing::error!(
        account = %account,
        subscription_ref,
        refused,
        "billing: could not end a subscription at the provider; it keeps charging until ended by hand"
    );
}

/// A refusal is checked against the provider's own view before it counts:
/// the second of a purchase's two events, a customer who cancelled through
/// the portal meanwhile, or an earlier attempt that landed there but not
/// here, all arrive after the subscription is already ending. A cancel
/// booked for the period end is refused a second one, unpaid or not, and
/// the caller decides whether that booked end is enough: the sweep takes
/// it, since a card retry that succeeds before the date brings the account
/// back live for the time it paid; the owner asking to end now is told.
/// Only an answer is worth checking; a timeout would time out again.
pub(super) async fn cancel_despite_refusal(
    provider: &dyn BillingProvider,
    subscription_ref: &str,
    timing: ChangeTiming,
) -> Result<SubscriptionSnapshot> {
    let refused = match provider.cancel(subscription_ref, timing).await {
        Err(err @ (AppError::NotFound { .. } | AppError::Conflict { .. })) => err,
        answer => return answer,
    };
    match provider.fetch_subscription(subscription_ref).await {
        Ok(seen) if seen.status == Remote::Canceled => {
            tracing::info!(
                subscription_ref,
                error = %refused,
                "billing: cancel refused for a subscription already ended at the provider"
            );
            Ok(seen)
        }
        Ok(seen) if seen.is_ending() => {
            if timing == ChangeTiming::Now {
                tracing::warn!(
                    subscription_ref,
                    error = %refused,
                    "billing: cancel refused; the subscription ends with its period and the card may be retried until then"
                );
            }
            Ok(seen)
        }
        _ => Err(refused),
    }
}

/// Every status is emitted, so one that empties reports 0 rather than its
/// last value.
async fn publish_status_gauge(pool: &PgPool) -> Result<()> {
    let counts = store::count_by_status(pool).await?;
    for status in BillingStatus::ALL {
        let n = counts
            .iter()
            .find(|(s, _)| s == status.as_db_str())
            .map_or(0, |(_, n)| *n);
        metrics::gauge!(names::SUBSCRIPTIONS, "status" => status.as_db_str()).set(n as f64);
    }
    Ok(())
}

/// An account with a live subscription answers only to that subscription.
/// Anything else, a payment naming no subscription included, is someone
/// else's purchase carrying this account's id, and must not be able to move
/// the plan or start a grace window.
fn binds(sub: &Subscription, subscription_ref: Option<&str>) -> bool {
    match (sub.status, sub.subscription_ref.as_deref()) {
        (BillingStatus::Active | BillingStatus::PastDue, Some(bound)) => {
            subscription_ref == Some(bound)
        }
        _ => true,
    }
}

/// Noise carries no account by design, and a subscription ended after its
/// account was purged charges nobody; neither is a stray purchase to chase.
/// A paused one resumes and charges, so it is.
fn unowned(kind: &EventKind) -> Outcome {
    match kind {
        EventKind::Other => Outcome::Applied,
        EventKind::Subscription(s) if s.status == Remote::Canceled => Outcome::Applied,
        _ => Outcome::Unmatched,
    }
}

/// Which reminder is owed, if the next one's day has come.
fn reminder_due(grace_until: DateTime<Utc>, now: DateTime<Utc>, sent: i16) -> Option<i16> {
    let started = grace_until - Duration::days(GRACE_DAYS);
    let elapsed = (now - started).num_days();
    let due = REMINDER_DAYS.iter().filter(|day| **day <= elapsed).count() as i16;
    (due > sent).then_some(due)
}

async fn record_booked(
    tx: &mut Transaction<'_, Postgres>,
    account: AccountId,
    price: &store::PlanPrice,
    at: DateTime<Utc>,
) -> Result<()> {
    ledger::record_tx(
        tx,
        account,
        ledger::DOWNGRADE_SCHEDULED,
        json!({ "to": price.plan_id, "at": at, "interval": price.interval }),
    )
    .await
}

/// A subscription already bound answers to its account whatever the event
/// says; the account the event names only claims a subscription nobody holds.
async fn resolve_account<'e, E: PgExecutor<'e>>(
    exec: E,
    provider: &str,
    event: &ProviderEvent,
) -> Result<Option<AccountId>> {
    if let Some(subscription_ref) = &event.subscription_ref
        && let Some(bound) =
            store::account_for_subscription_ref(exec, provider, subscription_ref).await?
    {
        if let Some(named) = event.account
            && named != bound
        {
            tracing::warn!(
                event_id = %event.event_id,
                subscription_ref,
                account = %bound,
                %named,
                "billing: event names another account than the one holding the subscription"
            );
        }
        return Ok(Some(bound));
    }
    Ok(event.account)
}

async fn plan_name<'e, E: PgExecutor<'e>>(exec: E, plan_id: &str) -> Result<String> {
    Ok(store::plan_brief(exec, plan_id)
        .await?
        .map_or_else(|| plan_id.to_owned(), |p| p.name))
}

/// Whether moving to `target` shrinks any of the pooled caps a hold is judged
/// against, which is what makes a change a downgrade to defer rather than an
/// upgrade to apply now. Shrinking on *any* axis counts, so a plan that trades
/// more of one resource for less of another is treated as a downgrade and
/// never cuts a resource before the period ends. An unknown plan counts as no
/// shrink, so the change applies now rather than being deferred on a guess.
pub(super) async fn smaller(
    conn: &mut sqlx::PgConnection,
    target: &str,
    current: &str,
) -> Result<bool> {
    let (Some(t), Some(c)) = (
        store::plan_brief(&mut *conn, target).await?,
        store::plan_brief(&mut *conn, current).await?,
    ) else {
        return Ok(false);
    };
    Ok(t.max_targets < c.max_targets
        || t.max_flow_checks < c.max_flow_checks
        || t.max_status_pages < c.max_status_pages)
}

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
struct PooledCounts {
    targets: i64,
    flows: i64,
    status_pages: i64,
}

impl PooledCounts {
    /// Mirrors the reconcile: flow monitors past the flow cap first, then
    /// whatever is left past the overall cap.
    fn monitors_over(&self, plan: &store::PlanBrief) -> i64 {
        let flows_over = (self.flows - i64::from(plan.max_flow_checks)).max(0);
        flows_over + (self.targets - flows_over - i64::from(plan.max_targets)).max(0)
    }
}

async fn pooled_counts(conn: &mut sqlx::PgConnection, account: AccountId) -> Result<PooledCounts> {
    use crate::quotas::service::count_sql as c;
    sqlx::query_as(&format!(
        "SELECT ({}) AS targets, ({}) AS flows, ({}) AS status_pages",
        c::targets(),
        c::flow(),
        c::status_pages()
    ))
    .bind(account.0)
    .fetch_one(conn)
    .await
    .context("billing: pooled counts")
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grace_from(start: DateTime<Utc>) -> DateTime<Utc> {
        start + Duration::days(GRACE_DAYS)
    }

    #[test]
    fn reminders_fall_on_the_scheduled_days_and_never_repeat() {
        let start = Utc::now();
        let until = grace_from(start);
        assert_eq!(reminder_due(until, start, 1), None);
        assert_eq!(reminder_due(until, start + Duration::days(2), 1), None);
        assert_eq!(reminder_due(until, start + Duration::days(3), 1), Some(2));
        assert_eq!(reminder_due(until, start + Duration::days(3), 2), None);
        assert_eq!(reminder_due(until, start + Duration::days(9), 2), Some(3));
        assert_eq!(reminder_due(until, start + Duration::days(13), 3), Some(4));
        assert_eq!(reminder_due(until, start + Duration::days(13), 4), None);
    }

    #[test]
    fn a_missed_tick_sends_one_reminder_not_the_backlog() {
        let start = Utc::now();
        let until = grace_from(start);
        assert_eq!(reminder_due(until, start + Duration::days(12), 1), Some(4));
    }

    fn sub(status: BillingStatus, bound: Option<&str>) -> Subscription {
        Subscription {
            account: AccountId(uuid::Uuid::nil()),
            owner: None,
            plan_id: "team".into(),
            fallback_plan_id: Some("founding".into()),
            status,
            pending_plan_id: None,
            plan_change_at: None,
            pending_interval: None,
            cancel_at: None,
            current_period_end: None,
            grace_until: None,
            dunning_stage: 0,
            provider: Some("fake".into()),
            customer_ref: None,
            subscription_ref: bound.map(str::to_owned),
            interval: None,
            synced_at: None,
            payment_synced_at: None,
        }
    }

    #[test]
    fn an_account_that_never_paid_is_shown_landing_where_it_stands() {
        let mut never = sub(BillingStatus::None, None);
        never.fallback_plan_id = None;
        assert_eq!(never.landing_plan(), "free");
        assert_eq!(never.landing_plan_preview(), "team");
        let paying = sub(BillingStatus::Active, Some("sub_1"));
        assert_eq!(paying.landing_plan_preview(), "founding");
    }

    #[test]
    fn a_downgrade_notice_counts_monitors_the_way_the_reconcile_holds_them() {
        let pro = store::PlanBrief {
            id: "pro".into(),
            name: "Pro".into(),
            max_targets: 50,
            max_flow_checks: 3,
            max_status_pages: 5,
        };
        let counts = PooledCounts {
            targets: 52,
            flows: 10,
            status_pages: 1,
        };
        assert_eq!(counts.monitors_over(&pro), 7);
        let counts = PooledCounts {
            targets: 60,
            flows: 10,
            status_pages: 1,
        };
        assert_eq!(counts.monitors_over(&pro), 7 + 3);
        let counts = PooledCounts {
            targets: 10,
            flows: 1,
            status_pages: 1,
        };
        assert_eq!(counts.monitors_over(&pro), 0);
    }

    #[test]
    fn a_live_subscription_answers_only_to_itself() {
        let live = sub(BillingStatus::Active, Some("sub_a"));
        assert!(binds(&live, Some("sub_a")));
        assert!(!binds(&live, Some("sub_b")));
        assert!(!binds(&live, None));
        let ended = sub(BillingStatus::Canceled, Some("sub_a"));
        assert!(binds(&ended, Some("sub_b")));
        assert!(binds(&sub(BillingStatus::None, None), Some("sub_b")));
    }
}
