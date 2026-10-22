//! The one send budget every instance draws from. The provider limits
//! requests per second per team, across keys and processes, so slots are
//! booked in one Postgres row that blue and green share. An instance that
//! cannot reach that row paces itself at the share that still fits beside a
//! healthy instance running flat out.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgConnection, PgPool};
use tokio::time::{Instant, sleep, timeout_at};

use super::trait_def::{
    BatchFailure, BatchResult, EmailError, EmailResult, EmailSender, MessageId, PreparedBatch,
    SendClass, TransactionalEmail,
};
use crate::metric_names;

/// Longest an Immediate send waits for its slot, whatever it waits on.
pub const IMMEDIATE_DEADLINE: Duration = Duration::from_secs(10);
/// Share of the team limit the shared row hands out.
const SHARED_SHARE: f64 = 0.8;
/// Share one instance on its fallback may use: with a second instance on the
/// shared row, the two together stay at the limit.
const FALLBACK_SHARE: f64 = 0.2;
/// A gate call gets this long, connection included, before the instance
/// gives up on the shared row.
const DB_CALL_BUDGET: Duration = Duration::from_secs(1);
/// How long an instance stays on its fallback before trying the row again,
/// so a stalled database costs one timeout per hold, not one per mail.
const FALLBACK_HOLD: Duration = Duration::from_secs(30);
/// Longest a Bulk send waits for a free slot. Past it the caller is told
/// when to come back, so a long provider block never holds a sweep, or the
/// claim it sends under, for the whole block.
pub const BULK_DEADLINE: Duration = Duration::from_secs(30);
/// Bulk waits at most this long between looks at a busy row.
const BULK_POLL_MAX: Duration = Duration::from_secs(1);
/// So Bulk callers that lost a race do not spin on the row.
const BULK_POLL_MIN: Duration = Duration::from_millis(10);
/// A wait shorter than this is no wait.
const NO_WAIT: Duration = Duration::from_millis(1);

/// One booking: the slot fits the deadline and is this far away, or it does
/// not and the next free one is this far away.
enum Booking {
    At(Duration),
    Full(Duration),
}

/// Why a gate call returned nothing.
enum Miss {
    /// The row could not be reached in time; this instance is on fallback.
    Down,
    /// The caller's own deadline ran out first.
    OutOfTime,
}

/// The fallback: a pacer inside the process.
struct LocalPacer {
    interval: Duration,
    next_free: Instant,
    blocked_until: Instant,
}

impl LocalPacer {
    fn book(&mut self, now: Instant, deadline: Instant) -> Booking {
        let free = self.next_free.max(self.blocked_until).max(now);
        if free > deadline {
            return Booking::Full(free - now);
        }
        self.next_free = free + self.interval;
        Booking::At(free - now)
    }

    fn blocked_for(&self, now: Instant) -> Duration {
        self.blocked_until.saturating_duration_since(now)
    }
}

pub struct SendGate {
    pool: PgPool,
    interval: Duration,
    local: Mutex<LocalPacer>,
    fallback_until: Mutex<Option<Instant>>,
    /// One Bulk caller per process looks at the row at a time; the rest wait
    /// here instead of all racing for each free slot.
    bulk_turn: tokio::sync::Mutex<()>,
    /// A 429 seen while on fallback, written to the row once it answers
    /// again so the other instance and this one's next booking honour it.
    unshared_block: Mutex<Option<Instant>>,
}

impl SendGate {
    /// Its own two connections, so a busy app pool cannot hold up an alert,
    /// and server-side limits so a stuck row lock or statement fails fast.
    /// Opens nothing until the first send.
    pub fn connect_lazy(database_url: &str, rate_limit_per_second: u32) -> sqlx::Result<Self> {
        let options: PgConnectOptions = database_url.parse()?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            // One kept open, so the first alert after a quiet spell does not
            // spend its budget on a connection handshake.
            .min_connections(1)
            .acquire_timeout(Duration::from_millis(250))
            // The client timeout below already catches a dead connection; a
            // ping per acquire would double every round trip.
            .test_before_acquire(false)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET statement_timeout = '500ms'").await?;
                    conn.execute("SET lock_timeout = '250ms'").await?;
                    Ok(())
                })
            })
            .connect_lazy_with(options);
        Ok(Self::new(pool, rate_limit_per_second))
    }

    fn new(pool: PgPool, rate_limit_per_second: u32) -> Self {
        let limit = f64::from(rate_limit_per_second.max(1));
        let now = Instant::now();
        Self {
            pool,
            interval: Duration::from_secs_f64(1.0 / (limit * SHARED_SHARE)),
            local: Mutex::new(LocalPacer {
                interval: Duration::from_secs_f64(1.0 / (limit * FALLBACK_SHARE)),
                next_free: now,
                blocked_until: now,
            }),
            fallback_until: Mutex::new(None),
            bulk_turn: tokio::sync::Mutex::new(()),
            unshared_block: Mutex::new(None),
        }
    }

    /// Waits for a slot that lands before `deadline`. `Err` carries how long
    /// until one would be free when none does.
    pub async fn immediate(&self, deadline: Instant) -> Result<(), Duration> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(Duration::from_secs(1));
            }
            let shared = !self.on_fallback();
            let booking = if shared {
                self.share_block().await;
                match self.book_shared(deadline, Some(deadline)).await {
                    Ok(booking) => booking,
                    Err(Miss::Down) => self.local.lock().book(Instant::now(), deadline),
                    Err(Miss::OutOfTime) => return Err(Duration::from_secs(1)),
                }
            } else {
                self.local.lock().book(now, deadline)
            };
            let wait = match booking {
                Booking::At(wait) => wait,
                // Lost the row to the other instance between snapshot and
                // lock: the free time read is stale, so book again.
                Booking::Full(free_in) if free_in < NO_WAIT => continue,
                Booking::Full(free_in) => return Err(free_in),
            };
            if wait < NO_WAIT {
                return Ok(());
            }
            sleep(wait).await;
            // A 429 that landed while this caller slept voids the slot: book
            // again behind the block. So does a row that stopped answering,
            // since a block another instance wrote there cannot be seen.
            let blocked = if shared && !self.on_fallback() {
                match self.shared_blocked_for(deadline).await {
                    Ok(blocked) => blocked,
                    Err(Miss::Down) => continue,
                    Err(Miss::OutOfTime) => return Err(Duration::from_secs(1)),
                }
            } else {
                self.local.lock().blocked_for(Instant::now())
            };
            if blocked < NO_WAIT {
                return Ok(());
            }
        }
    }

    /// Takes a slot only when nobody holds the next one, so Immediate mail
    /// never queues behind a sweep. Gives up at `deadline`, saying when a slot
    /// would be free.
    pub async fn bulk(&self, deadline: Instant) -> EmailResult<()> {
        let _turn = tokio::time::timeout_at(deadline, self.bulk_turn.lock())
            .await
            .map_err(|_| EmailError::Deferred {
                retry_after: Duration::from_secs(1),
            })?;
        loop {
            if self.on_fallback() {
                return Err(EmailError::GateUnavailable);
            }
            self.share_block().await;
            // A horizon of now takes only a slot nobody holds.
            let free_in = match self.book_shared(Instant::now(), None).await {
                Ok(Booking::At(_)) => return Ok(()),
                Ok(Booking::Full(free_in)) => free_in,
                Err(_) => return Err(EmailError::GateUnavailable),
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if free_in > left {
                return Err(EmailError::Deferred {
                    retry_after: free_in,
                });
            }
            sleep(free_in.clamp(BULK_POLL_MIN, BULK_POLL_MAX).min(left)).await;
        }
    }

    /// The provider asked everyone to wait.
    pub async fn block(&self, retry_after: Duration) {
        let until = Instant::now() + retry_after;
        {
            let mut local = self.local.lock();
            local.blocked_until = local.blocked_until.max(until);
        }
        if self.on_fallback() || self.write_block(until).await.is_err() {
            let mut unshared = self.unshared_block.lock();
            *unshared = Some(unshared.map_or(until, |u| u.max(until)));
        }
    }

    /// Hands a block seen while on fallback to the row.
    async fn share_block(&self) {
        let Some(until) = self.unshared_block.lock().take() else {
            return;
        };
        if until > Instant::now() && self.write_block(until).await.is_err() {
            let mut unshared = self.unshared_block.lock();
            *unshared = Some(unshared.map_or(until, |u| u.max(until)));
        }
    }

    async fn write_block(&self, until: Instant) -> Result<(), Miss> {
        let secs = until
            .saturating_duration_since(Instant::now())
            .as_secs_f64();
        self.db(None, async |conn: &mut PgConnection| {
            sqlx::query(
                "UPDATE email_send_gate \
                 SET blocked_until = GREATEST(blocked_until, \
                                              clock_timestamp() + make_interval(secs => $1)) \
                 WHERE id",
            )
            .bind(secs)
            .execute(conn)
            .await
            .map(|_| ())
        })
        .await
    }

    /// Books the next slot if it lands by `horizon`. `call_deadline` bounds
    /// the database call itself when the caller has one.
    async fn book_shared(
        &self,
        horizon: Instant,
        call_deadline: Option<Instant>,
    ) -> Result<Booking, Miss> {
        let interval = self.interval.as_secs_f64();
        self.db(call_deadline, async |conn: &mut PgConnection| {
            // Measured just before the statement, so no booking reaches past
            // the deadline the send started with.
            let budget = horizon
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            let (booked, free_in): (Option<f64>, f64) = sqlx::query_as(
                "WITH booked AS (
                     UPDATE email_send_gate
                     SET next_free_at = GREATEST(next_free_at, clock_timestamp(), blocked_until)
                                        + make_interval(secs => $1)
                     WHERE id
                       AND GREATEST(next_free_at, blocked_until)
                           <= clock_timestamp() + make_interval(secs => $2)
                     RETURNING next_free_at - make_interval(secs => $1) AS slot
                 )
                 SELECT (SELECT GREATEST(0, EXTRACT(EPOCH FROM slot - clock_timestamp()))::float8
                         FROM booked),
                        GREATEST(0, EXTRACT(EPOCH FROM GREATEST(g.next_free_at, g.blocked_until)
                                                    - clock_timestamp()))::float8
                 FROM email_send_gate g",
            )
            .bind(interval)
            .bind(budget)
            .fetch_one(conn)
            .await?;
            Ok(match booked {
                Some(wait) => Booking::At(seconds(wait)),
                None => Booking::Full(seconds(free_in)),
            })
        })
        .await
    }

    async fn shared_blocked_for(&self, deadline: Instant) -> Result<Duration, Miss> {
        self.db(Some(deadline), async |conn: &mut PgConnection| {
            let (secs,): (f64,) = sqlx::query_as(
                "SELECT GREATEST(0, EXTRACT(EPOCH FROM blocked_until - clock_timestamp()))::float8 \
                 FROM email_send_gate",
            )
            .fetch_one(conn)
            .await?;
            Ok(seconds(secs))
        })
        .await
    }

    /// One gate statement within [`DB_CALL_BUDGET`] or the caller's
    /// deadline, whichever comes first. Failing within the budget puts the
    /// instance on fallback; running into the caller's own deadline does not.
    async fn db<T>(
        &self,
        deadline: Option<Instant>,
        op: impl AsyncFnOnce(&mut PgConnection) -> sqlx::Result<T>,
    ) -> Result<T, Miss> {
        let budget_end = Instant::now() + DB_CALL_BUDGET;
        let (at, cut_by_caller) = match deadline {
            Some(d) if d < budget_end => (d, true),
            _ => (budget_end, false),
        };
        let mut conn = match timeout_at(at, self.pool.acquire()).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(self.trip(&e.to_string())),
            Err(_) if cut_by_caller => return Err(Miss::OutOfTime),
            Err(_) => return Err(self.trip("no gate connection in time")),
        };
        match timeout_at(at, op(&mut *conn)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(self.trip(&e.to_string())),
            Err(_) => {
                // Whatever the server still does with it, this connection
                // is not handed to the next caller.
                conn.close_on_drop();
                if cut_by_caller {
                    Err(Miss::OutOfTime)
                } else {
                    Err(self.trip("gate statement timed out"))
                }
            }
        }
    }

    fn on_fallback(&self) -> bool {
        self.fallback_until
            .lock()
            .is_some_and(|until| Instant::now() < until)
    }

    fn trip(&self, why: &str) -> Miss {
        let now = Instant::now();
        let mut until = self.fallback_until.lock();
        let entering = until.is_none_or(|u| u <= now);
        *until = Some(now + FALLBACK_HOLD);
        if entering {
            tracing::warn!(
                error = %why,
                hold_secs = FALLBACK_HOLD.as_secs(),
                "email send gate unreachable; pacing this instance on its own"
            );
            metrics::counter!(metric_names::EMAIL_GATE_FALLBACK).increment(1);
        }
        Miss::Down
    }
}

fn seconds(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// The provider behind the gate. Every send takes a slot first; a 429
/// blocks every instance for the wait it names.
pub struct GatedEmailSender {
    inner: Arc<dyn EmailSender>,
    gate: SendGate,
}

impl GatedEmailSender {
    pub fn new(inner: Arc<dyn EmailSender>, gate: SendGate) -> Self {
        Self { inner, gate }
    }

    async fn send_immediate(
        &self,
        email: TransactionalEmail,
        deadline: Instant,
    ) -> EmailResult<MessageId> {
        let mut refused: Option<EmailError> = None;
        loop {
            let waited = Instant::now();
            if let Err(free_in) = self.gate.immediate(deadline).await {
                // A provider 429 already in hand says more than our own
                // budget running out behind it.
                return Err(refused.unwrap_or(EmailError::Deferred {
                    retry_after: free_in,
                }));
            }
            note_wait(SendClass::Immediate, waited);
            match self.inner.send(email.clone(), SendClass::Immediate).await {
                Err(e @ EmailError::RateLimited { .. }) => {
                    if let EmailError::RateLimited { retry_after, .. } = &e {
                        self.gate.block(*retry_after).await;
                    }
                    if refused.is_some() {
                        return Err(e);
                    }
                    // Once, and only if a slot behind the block still fits
                    // the deadline the send started with.
                    refused = Some(e);
                }
                other => return other,
            }
        }
    }

    async fn send_bulk(&self, email: TransactionalEmail) -> EmailResult<MessageId> {
        let waited = Instant::now();
        self.gate.bulk(waited + BULK_DEADLINE).await?;
        note_wait(SendClass::Bulk, waited);
        let sent = self.inner.send(email, SendClass::Bulk).await;
        if let Err(EmailError::RateLimited { retry_after, .. }) = &sent {
            self.gate.block(*retry_after).await;
        }
        sent
    }
}

fn note_wait(class: SendClass, since: Instant) {
    metrics::histogram!(metric_names::EMAIL_GATE_WAIT_MS, "class" => class.as_label())
        .record(since.elapsed().as_millis() as f64);
}

fn note_outcome(class: SendClass, outcome: Result<(), &EmailError>) {
    let label = match outcome {
        Ok(()) => "sent",
        Err(e) => {
            if matches!(e, EmailError::QuotaExceeded { .. } | EmailError::Refused(_)) {
                // Only the operator can fix these, and every mail fails
                // until they do.
                tracing::error!(error = %e, "email provider will not take our mail");
            }
            e.as_label()
        }
    };
    metrics::counter!(
        metric_names::EMAIL_SENDS,
        "class" => class.as_label(),
        "outcome" => label
    )
    .increment(1);
}

#[async_trait]
impl EmailSender for GatedEmailSender {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId> {
        let sent = match class {
            SendClass::Immediate => {
                self.send_immediate(email, Instant::now() + IMMEDIATE_DEADLINE)
                    .await
            }
            SendClass::Bulk => self.send_bulk(email).await,
        };
        note_outcome(class, sent.as_ref().map(|_| ()));
        sent
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        self.inner.prepare_batch(emails, key)
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        let waited = Instant::now();
        if let Err(error) = self.gate.bulk(waited + BULK_DEADLINE).await {
            note_outcome(SendClass::Bulk, Err(&error));
            return Err(BatchFailure { error, batch });
        }
        note_wait(SendClass::Bulk, waited);
        let sent = self.inner.send_prepared(batch).await;
        match &sent {
            Ok(_) => note_outcome(SendClass::Bulk, Ok(())),
            Err(failure) => {
                if let EmailError::RateLimited { retry_after, .. } = &failure.error {
                    self.gate.block(*retry_after).await;
                }
                note_outcome(SendClass::Bulk, Err(&failure.error));
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pacer(interval_ms: u64) -> LocalPacer {
        let now = Instant::now();
        LocalPacer {
            interval: Duration::from_millis(interval_ms),
            next_free: now,
            blocked_until: now,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_fallback_spaces_slots_and_refuses_past_the_deadline() {
        let mut p = pacer(500);
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        assert!(matches!(p.book(now, deadline), Booking::At(w) if w.is_zero()));
        assert!(matches!(p.book(now, deadline), Booking::At(w) if w == Duration::from_millis(500)));
        assert!(matches!(p.book(now, deadline), Booking::At(w) if w == Duration::from_secs(1)));
        assert!(
            matches!(p.book(now, deadline), Booking::Full(w) if w == Duration::from_millis(1500))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_block_on_the_fallback_moves_the_next_slot_behind_it() {
        let mut p = pacer(500);
        let now = Instant::now();
        p.blocked_until = now + Duration::from_secs(2);
        assert_eq!(p.blocked_for(now), Duration::from_secs(2));
        let deadline = now + Duration::from_secs(10);
        assert!(matches!(p.book(now, deadline), Booking::At(w) if w == Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn shares_keep_a_healthy_and_a_cut_off_instance_at_the_limit() {
        let gate = SendGate::new(
            PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            10,
        );
        let shared_per_sec = 1.0 / gate.interval.as_secs_f64();
        let fallback_per_sec = 1.0 / gate.local.lock().interval.as_secs_f64();
        assert!((shared_per_sec - 8.0).abs() < 1e-9);
        assert!((fallback_per_sec - 2.0).abs() < 1e-9);
        assert!(shared_per_sec + fallback_per_sec <= 10.0 + 1e-9);
    }

    #[test]
    fn odd_or_garbage_seconds_read_as_no_wait() {
        assert_eq!(seconds(f64::NAN), Duration::ZERO);
        assert_eq!(seconds(f64::NEG_INFINITY), Duration::ZERO);
        assert_eq!(seconds(-0.2), Duration::ZERO);
        assert_eq!(seconds(0.25), Duration::from_millis(250));
    }
}
