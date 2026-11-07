//! What alert ingest may cost the rest of the service: a rate per presented
//! token, few token lookups and ingest transactions at once whatever the load,
//! body memory bounded from the first byte read until the body's events are
//! applied, requests per org bounded so one org cannot hold that memory, and a
//! bounded number of delivery-log writes in flight.

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use bytes::BytesMut;
use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};
use uuid::Uuid;

use crate::security::token_limiter::TokenLimiter;

/// The largest body read, the same as the API's bulk routes. The edge refuses
/// larger ones on their declared length first.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Per token. The burst covers a mass event in Datadog, which sends one
/// request per monitor.
const PER_SEC: u32 = 10;
const BURST: u32 = 300;
/// Body bytes held at once, read or waiting to be applied, in KiB.
const BODY_BUDGET_KIB: usize = 128 * 1024;
/// Requests per org from the first byte read until their events are applied.
const REQUESTS_PER_ORG: usize = 4;
/// A trickled body would otherwise hold its org's turn for as long as it
/// likes. Waiting for memory counts against it too.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);
/// Token lookups at once: a flood of made-up tokens passes the per-token
/// rate, since each is new, so this is what keeps it off the pool.
const CONCURRENT_LOOKUPS: usize = 4;
/// Ingest transactions at once. Each holds a pool connection, so a storm can
/// never take the pool from the rest of the service.
const CONCURRENT_WRITES: usize = 4;
/// Past this a request answers 503 and the sender retries, rather than
/// queueing behind a database that is not keeping up.
const DB_WAIT: Duration = Duration::from_secs(10);
/// Delivery-log writes in flight; past it a request's log rows are dropped.
const CONCURRENT_LOG_WRITES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    TooLarge,
    /// Not finished inside the deadline.
    Stalled,
    /// The sender went away mid-body.
    Broken,
    /// The org's earlier requests are still being applied.
    Busy,
}

pub struct IntakeGate {
    tokens: TokenLimiter,
    /// One unavailable line a minute per source in its delivery log.
    unavailable: TokenLimiter,
    lookups: Semaphore,
    budget: Arc<Semaphore>,
    org_requests: KeyedSlots<Uuid>,
    deadline: Duration,
    writes: Semaphore,
    source_turns: KeyedSlots<Uuid>,
    log_writes: Arc<Semaphore>,
}

impl Default for IntakeGate {
    fn default() -> Self {
        Self::with_deadline(READ_DEADLINE)
    }
}

/// A body read whole. Holding it keeps its org's turn and its memory
/// counted; drop it once its events are applied.
pub struct Read<'a> {
    pub body: Bytes,
    _org: Slot<'a, Uuid>,
    _memory: Option<OwnedSemaphorePermit>,
}

impl IntakeGate {
    pub fn with_deadline(deadline: Duration) -> Self {
        Self {
            tokens: TokenLimiter::new(PER_SEC, BURST),
            unavailable: TokenLimiter::per_minute(1),
            lookups: Semaphore::new(CONCURRENT_LOOKUPS),
            budget: Arc::new(Semaphore::new(BODY_BUDGET_KIB)),
            org_requests: KeyedSlots::new(REQUESTS_PER_ORG),
            deadline,
            writes: Semaphore::new(CONCURRENT_WRITES),
            source_turns: KeyedSlots::new(1),
            log_writes: Arc::new(Semaphore::new(CONCURRENT_LOG_WRITES)),
        }
    }

    /// Admits one request for the token behind `key`, or says how long until
    /// it may send again.
    pub fn admit(&self, key: u128) -> Result<(), Duration> {
        self.tokens.check(key)
    }

    /// Whether a request the database failed should leave a line in its
    /// source's delivery log: one a minute, so an outage the sender keeps
    /// retrying through does not bury the log.
    pub fn logs_unavailable(&self, source: Uuid) -> bool {
        self.unavailable.check(source.as_u128()).is_ok()
    }

    /// A turn to look a token up, or `None` when lookups are backed up.
    pub async fn lookup(&self) -> Option<SemaphorePermit<'_>> {
        tokio::time::timeout(DB_WAIT, self.lookups.acquire())
            .await
            .ok()?
            .ok()
    }

    /// Reads the whole body on its org's turn, refusing one declared or found
    /// over [`MAX_BODY_BYTES`]. A declared length is counted against the
    /// budget up front, so bodies never jam each other holding part of it;
    /// an undeclared one is counted per KiB as it arrives, so a trickled body
    /// holds only what it sent.
    pub async fn read(
        &self,
        org: Uuid,
        body: Body,
        declared: Option<u64>,
    ) -> Result<Read<'_>, ReadError> {
        let declared = match declared.map(usize::try_from) {
            Some(Ok(n)) if n <= MAX_BODY_BYTES => Some(n),
            Some(_) => return Err(ReadError::TooLarge),
            None => None,
        };
        let org = tokio::time::timeout(DB_WAIT, self.org_requests.acquire(org))
            .await
            .map_err(|_| ReadError::Busy)?;
        let read = async {
            let mut memory = Memory::default();
            self.count(&mut memory, declared.unwrap_or(0)).await?;
            let mut buf = BytesMut::with_capacity(declared.unwrap_or(0));
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| ReadError::Broken)?;
                let len = buf.len() + chunk.len();
                if len > MAX_BODY_BYTES {
                    return Err(ReadError::TooLarge);
                }
                self.count(&mut memory, len).await?;
                buf.extend_from_slice(&chunk);
            }
            Ok(Read {
                body: buf.freeze(),
                _org: org,
                _memory: memory.held,
            })
        };
        tokio::time::timeout(self.deadline, read)
            .await
            .unwrap_or(Err(ReadError::Stalled))
    }

    /// Counts a body of `bytes` against the budget, taking only what is not
    /// counted yet.
    async fn count(&self, memory: &mut Memory, bytes: usize) -> Result<(), ReadError> {
        let more = bytes.div_ceil(1024).saturating_sub(memory.kib);
        if more == 0 {
            return Ok(());
        }
        let units = u32::try_from(more).map_err(|_| ReadError::TooLarge)?;
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(units)
            .await
            .map_err(|_| ReadError::Broken)?;
        match memory.held.as_mut() {
            Some(held) => held.merge(permit),
            None => memory.held = Some(permit),
        }
        memory.kib += more;
        Ok(())
    }

    /// The source's turn for one request's events, so a burst on one source
    /// queues here without holding pool connections while it waits on the
    /// source's advisory lock. `None` when it did not come in time.
    pub async fn source_turn(&self, source: Uuid) -> Option<Slot<'_, Uuid>> {
        tokio::time::timeout(DB_WAIT, self.source_turns.acquire(source))
            .await
            .ok()
    }

    /// A turn for one ingest transaction, or `None` when the database is not
    /// keeping up.
    pub async fn write(&self) -> Option<SemaphorePermit<'_>> {
        tokio::time::timeout(DB_WAIT, self.writes.acquire())
            .await
            .ok()?
            .ok()
    }

    /// A slot for one delivery-log write, or `None` when too many are in
    /// flight already, as when ClickHouse is slow.
    pub fn log_write(&self) -> Option<OwnedSemaphorePermit> {
        self.log_writes.clone().try_acquire_owned().ok()
    }
}

/// What a body being read holds of the budget.
#[derive(Default)]
struct Memory {
    held: Option<OwnedSemaphorePermit>,
    kib: usize,
}

/// At most `per_key` holders per key at once. A key's entry is dropped with
/// its last holder or waiter, a cancelled one included, so keys seen once do
/// not accumulate.
struct KeyedSlots<K: Eq + Hash + Clone> {
    per_key: usize,
    map: DashMap<K, Arc<Semaphore>>,
}

/// A held or awaited turn. It owns the only clone of the key's semaphore it
/// uses, so the map's count of clones is exactly its holders and waiters.
pub struct Slot<'a, K: Eq + Hash + Clone> {
    sem: Option<Arc<Semaphore>>,
    held: bool,
    slots: &'a KeyedSlots<K>,
    key: K,
}

impl<K: Eq + Hash + Clone> KeyedSlots<K> {
    fn new(per_key: usize) -> Self {
        Self {
            per_key,
            map: DashMap::new(),
        }
    }

    async fn acquire(&self, key: K) -> Slot<'_, K> {
        let sem = self
            .map
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_key)))
            .clone();
        let mut slot = Slot {
            sem: Some(sem),
            held: false,
            slots: self,
            key,
        };
        if let Some(sem) = slot.sem.as_deref()
            && let Ok(permit) = sem.acquire().await
        {
            permit.forget();
            slot.held = true;
        }
        slot
    }
}

impl<K: Eq + Hash + Clone> Drop for Slot<'_, K> {
    fn drop(&mut self) {
        if let Some(sem) = self.sem.take()
            && self.held
        {
            sem.add_permits(1);
        }
        self.slots
            .map
            .remove_if(&self.key, |_, sem| Arc::strong_count(sem) == 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> Body {
        Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>())
    }

    #[test]
    fn a_failing_source_logs_once_a_minute() {
        let gate = IntakeGate::default();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(gate.logs_unavailable(a));
        assert!(!gate.logs_unavailable(a), "once a minute");
        assert!(gate.logs_unavailable(b), "each source its own");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_declared_or_not() {
        let gate = IntakeGate::default();
        let org = Uuid::new_v4();
        let big = vec![b'x'; MAX_BODY_BYTES + 1];
        assert_eq!(
            gate.read(org, Body::empty(), Some(MAX_BODY_BYTES as u64 + 1))
                .await
                .err(),
            Some(ReadError::TooLarge)
        );
        assert_eq!(
            gate.read(org, Body::from(big), None).await.err(),
            Some(ReadError::TooLarge)
        );
        let fits = gate.read(org, Body::from("{}"), Some(2)).await.unwrap();
        assert_eq!(&fits.body[..], b"{}");
        assert_eq!(gate.budget.available_permits(), BODY_BUDGET_KIB - 1);
        drop(fits);
        assert_eq!(gate.budget.available_permits(), BODY_BUDGET_KIB);
        assert!(
            gate.org_requests.map.is_empty(),
            "a finished read leaves no entry"
        );
    }

    #[tokio::test]
    async fn a_body_that_never_finishes_stalls_at_the_deadline_and_leaves_nothing() {
        let gate = IntakeGate::with_deadline(Duration::from_millis(50));
        assert_eq!(
            gate.read(Uuid::new_v4(), pending(), None).await.err(),
            Some(ReadError::Stalled)
        );
        assert!(gate.org_requests.map.is_empty());
    }

    #[tokio::test]
    async fn one_org_holding_its_turns_leaves_room_for_others() {
        let gate = Arc::new(IntakeGate::with_deadline(Duration::from_millis(300)));
        let greedy = Uuid::new_v4();
        let held: Vec<_> = (0..REQUESTS_PER_ORG * 4)
            .map(|_| {
                let gate = gate.clone();
                tokio::spawn(async move { gate.read(greedy, pending(), None).await.err() })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let other = gate.read(Uuid::new_v4(), Body::from("{}"), None).await;
        assert!(other.is_ok(), "another org reads while one trickles");
        drop(other);
        for h in held {
            assert_eq!(h.await.unwrap(), Some(ReadError::Stalled));
        }
        assert!(
            gate.org_requests.map.is_empty(),
            "cancelled waiters leave no entry"
        );
    }

    #[tokio::test]
    async fn one_source_takes_turns_and_a_cancelled_waiter_leaves_no_entry() {
        let gate = IntakeGate::default();
        let busy = Uuid::new_v4();
        let first = gate.source_turn(busy).await.expect("a free turn");
        let second = tokio::time::timeout(Duration::from_millis(50), gate.source_turn(busy)).await;
        assert!(second.is_err(), "the same source waits its turn");
        assert!(gate.source_turn(Uuid::new_v4()).await.is_some());
        drop(first);
        assert!(gate.source_turns.map.is_empty());
        assert!(gate.source_turn(busy).await.is_some());
        assert!(gate.source_turns.map.is_empty());
    }
}
