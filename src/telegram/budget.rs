//! Local send budget for the central bot. Telegram rate-limits per bot
//! (~30 msg/s bot-wide, ~1 msg/s per chat, ~20 msg/min per group) and the
//! central bot shares that budget across every org — metering locally keeps
//! sends under the ceiling so remote 429s (which burn retry attempts at the
//! worst moment) never happen. BYO bots are not metered: each customer's
//! bot has its own budget.

use std::collections::HashMap;

use parking_lot::Mutex;
use tokio::time::{Duration, Instant};

/// Headroom under Telegram's 30 msg/s, so webhook replies racing engine
/// pages can't tip the real ceiling.
const GLOBAL_RATE_PER_SEC: f64 = 25.0;
/// How far the global cursor may lag behind now — i.e. the instant burst.
const GLOBAL_BURST: f64 = 25.0;
const PRIVATE_CHAT_INTERVAL: Duration = Duration::from_secs(1);
/// Groups allow ~20 msg/min sustained.
const GROUP_CHAT_INTERVAL: Duration = Duration::from_secs(3);
/// Longest a caller is held; a longer projected wait is returned as a
/// deferral so the escalation engine can reschedule instead of stalling.
const MAX_WAIT: Duration = Duration::from_secs(10);
/// Stale per-chat entries are swept once the map outgrows this.
const CHAT_MAP_SWEEP_LEN: usize = 8_192;

/// The projected wait exceeds [`MAX_WAIT`]; retry after this many seconds.
/// Carried into the delivery error as a `"retry_after":N` fragment so the
/// engine's existing vendor-hint path schedules the retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDeferred {
    pub retry_after_secs: u64,
}

struct BudgetState {
    /// GCRA-style cursor: total reserved send-time so far. May run ahead of
    /// `now` by up to the burst window without delaying anyone.
    global_cursor: Instant,
    /// Earliest next send per chat.
    chat_next: HashMap<i64, Instant>,
}

pub struct TelegramSendBudget {
    state: Mutex<BudgetState>,
}

impl Default for TelegramSendBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl TelegramSendBudget {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BudgetState {
                global_cursor: Instant::now(),
                chat_next: HashMap::new(),
            }),
        }
    }

    /// Reserve a send slot for `chat_id` and wait until it arrives. Returns
    /// [`RateDeferred`] without reserving anything when the wait would
    /// exceed [`MAX_WAIT`]. A caller cancelled mid-wait wastes its slot —
    /// the budget under-sends, never over-sends.
    pub async fn acquire(&self, chat_id: i64) -> Result<(), RateDeferred> {
        let slot = self.reserve(chat_id).inspect_err(|_| {
            metrics::counter!(crate::metric_names::TELEGRAM_SEND_DEFERRED).increment(1);
        })?;
        let now = Instant::now();
        if slot > now {
            metrics::histogram!(crate::metric_names::TELEGRAM_SEND_WAIT_MS)
                .record((slot - now).as_millis() as f64);
            tokio::time::sleep(slot - now).await;
        }
        Ok(())
    }

    fn reserve(&self, chat_id: i64) -> Result<Instant, RateDeferred> {
        let interval = if chat_id < 0 {
            GROUP_CHAT_INTERVAL
        } else {
            PRIVATE_CHAT_INTERVAL
        };
        let now = Instant::now();
        let burst_window = Duration::from_secs_f64(GLOBAL_BURST / GLOBAL_RATE_PER_SEC);
        let mut s = self.state.lock();

        // Sends are free while the cursor stays within the burst window of
        // `now`; past it, each send waits for the cursor's excess.
        let global_slot = s
            .global_cursor
            .checked_sub(burst_window)
            .map_or(now, |credited| now.max(credited));
        let chat_slot = s.chat_next.get(&chat_id).copied().unwrap_or(now);
        let slot = global_slot.max(chat_slot);

        let wait = slot.saturating_duration_since(now);
        if wait > MAX_WAIT {
            // Nothing reserved — the caller reschedules and re-reserves.
            return Err(RateDeferred {
                retry_after_secs: wait.as_secs() + 1,
            });
        }

        s.global_cursor =
            s.global_cursor.max(now) + Duration::from_secs_f64(1.0 / GLOBAL_RATE_PER_SEC);
        s.chat_next.insert(chat_id, slot + interval);
        if s.chat_next.len() > CHAT_MAP_SWEEP_LEN {
            s.chat_next.retain(|_, next| *next > now);
        }
        Ok(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn burst_within_budget_is_immediate() {
        let b = TelegramSendBudget::new();
        let start = Instant::now();
        for chat in 1..=20 {
            b.acquire(chat).await.unwrap();
        }
        assert_eq!(Instant::now(), start, "distinct chats inside the burst");
    }

    #[tokio::test(start_paused = true)]
    async fn same_chat_sends_are_spaced() {
        let b = TelegramSendBudget::new();
        let start = Instant::now();
        b.acquire(42).await.unwrap();
        b.acquire(42).await.unwrap();
        assert!(Instant::now() - start >= PRIVATE_CHAT_INTERVAL);

        let group_start = Instant::now();
        b.acquire(-100).await.unwrap();
        b.acquire(-100).await.unwrap();
        assert!(Instant::now() - group_start >= GROUP_CHAT_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn global_rate_paces_past_the_burst() {
        let b = TelegramSendBudget::new();
        let start = Instant::now();
        // Distinct chats so only the global cursor gates: the burst absorbs
        // the first 25, the next 25 drain at 25/s.
        for chat in 1..=50 {
            b.acquire(chat).await.unwrap();
        }
        let elapsed = Instant::now() - start;
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed <= Duration::from_millis(1300), "{elapsed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn long_wait_defers_without_reserving() {
        let b = TelegramSendBudget::new();
        // Book the group chat solid without waiting the reservations out:
        // slots land at 0/3/6/9s; the next would be 12s — past MAX_WAIT.
        for _ in 0..4 {
            b.reserve(-7).unwrap();
        }
        let err = b.reserve(-7).expect_err("must defer");
        assert!(err.retry_after_secs >= 10, "{err:?}");
        // The deferral reserved nothing: a different chat is still instant.
        let start = Instant::now();
        b.acquire(1).await.unwrap();
        assert_eq!(Instant::now(), start);
    }
}
