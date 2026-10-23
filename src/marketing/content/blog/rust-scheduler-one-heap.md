+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "Scheduling in Rust: one heap, not a timer per task"
date = "2026-08-05"
slug = "rust-scheduler-one-heap"
excerpt = "Every check my uptime monitor runs is scheduled from one BinaryHeap on one task. No timers, no locks, no drift. Five things that fell out of building it."
tags = ["rust", "tokio", "scheduling", "performance"]
draft = false
+++

> **TL;DR.** Thousands of monitors, each on its own interval, all scheduled from one `BinaryHeap<Reverse<Due>>` owned by a single task. Cancelling a job never touches the heap, it bumps a generation counter and lets the stale entry die on pop. Start times are a hash of the id, so a thousand monitors on a 60-second interval do not all fire on the same second. Falling behind skips the backlog instead of replaying it. And the one slow thing, reading config from Postgres, runs somewhere else entirely.

Every monitor has an interval. Some check every 20 seconds, some every 5 minutes, some once a day. They get added, edited and deleted while the process runs. Nothing may drift, and nothing may fire twice.

The obvious way to write that is a task per monitor:

```rust
// The version I did not ship.
for target in targets {
    tokio::spawn(async move {
        let mut tick = interval(target.interval);
        loop {
            tick.tick().await;
            probe(&target).await;
        }
    });
}
```

I did not ship it. Not because spawning is expensive: a Tokio task is cheap, and thousands of them are fine. The problem is everything around that loop.

Replacing it with one heap taught me five things.

## 1. The cost of a task per job is not the task

Count what you own in that first version. Memory grows with the fleet, which is fine. But so does the number of independent clocks, and that is not.

Editing a monitor means finding its task and stopping it. So now you keep a map of ids to `JoinHandle`s, and a lock around the map, and you think about what happens when a probe is in flight while the edit lands. Deleting is the same problem again. Each task also holds its own copy of the config, so "what is this monitor set to right now" has as many answers as you have tasks.

None of that is the scheduling problem. It is bookkeeping you inherit by spreading one decision, what runs next, across a thousand places.

One heap puts the decision back in one place:

```rust
struct Due {
    at: Instant,
    id: Uuid,
    seq: u64,
    first: bool,
}

let mut heap: BinaryHeap<Reverse<Due>> = BinaryHeap::new();
let mut live: HashMap<Uuid, u64> = HashMap::new();
```

`BinaryHeap` is a max-heap, so entries go in wrapped in `Reverse` to pop the earliest first. The driver is one loop that sleeps until the top of the heap is due:

```rust
loop {
    let next_at = heap.peek().map(|Reverse(due)| due.at);
    tokio::select! {
        _ = shutdown.cancelled() => break,
        maybe = diff_rx.recv() => match maybe {
            Some(diff) => self.apply_diff(&diff, &mut heap, &mut live, &mut next_seq),
            None => break,
        },
        _ = sweep.tick() => self.sweep_once(),
        _ = sleep_until_opt(next_at) => self.drain_due(&mut heap, &mut live),
    }
}
```

Because `heap` and `live` never leave this task, neither one needs a lock or an `Arc`. They are plain local variables in a `loop`. Every question about what runs next has exactly one answer, held by one owner.

One small thing that is easy to get wrong: an empty heap has no next time, and a `select!` branch that returns instantly turns the loop into a spin.

```rust
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}
```

`pending()` is a future that never resolves. With no work scheduled, that branch never wins, and the loop waits on the others.

## 2. You cannot remove from a heap, so do not try

A `BinaryHeap` gives you push and pop. There is no "delete this entry in the middle". When a monitor is edited or deleted, its pending entry is already inside, and you cannot reach in and pull it out.

The usual workaround is to rebuild the heap on every change. Do not. The fix is to stop treating the heap as the truth.

Each entry carries a generation number, `seq`. A separate map holds the current generation for each live id. Rescheduling means writing a new number:

```rust
fn schedule(st: &ScheduledTarget, heap: &mut …, live: &mut …, next_seq: &mut u64, now: Instant) {
    let id = st.target.id;
    let seq = *next_seq;
    *next_seq = next_seq.wrapping_add(1);
    live.insert(id, seq);
    // …push a fresh Due { at, id, seq, first: true }
}
```

The old entry is still in the heap. It is just no longer the current generation, and the pop side is where that gets decided:

```rust
let Reverse(due) = heap.pop().expect("peeked a due entry");
if live.get(&due.id) != Some(&due.seq) {
    continue;
}
```

A superseded entry fails that check and is dropped. A deleted monitor is removed from `live`, so `get` returns `None` and every entry it left behind dies the same way. Nothing scans the heap, nothing rebuilds it, nothing locks.

The cost is that a stale entry lingers until its time comes around. That is bounded by the interval it was scheduled at, and it is a few dozen bytes. Cheap. Watch one edit and one delete work their way out:

<div class="mk-embed-supersede"></div>

*Upper rail: entries still on the heap, each parked at the second it is due. Lower rail: what the pop decided once the clock reached it. Watch B get edited at t+8s and C deleted at t+32s, and the entries they leave behind.*

This pattern is worth stealing for any queue you cannot delete from: mark the truth somewhere else and let the queue lie until you pop it.

## 3. A hashed offset, or everything fires on the same second

A thousand monitors on a 60-second interval, all scheduled at boot, are a thousand probes on the same second, then 59 seconds of nothing, forever. Your outbound connections spike, your database write batches spike, and your own latency numbers get worse at exactly the moment you are measuring someone else.

So every target gets a fixed offset inside its own interval, derived from its id:

```rust
fn stagger_offset(id: Uuid, interval: Duration) -> Duration {
    let interval_ms = interval.as_millis() as u64;
    if interval_ms == 0 {
        return Duration::ZERO;
    }
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    Duration::from_millis(h.finish() % interval_ms)
}
```

Two things matter here. The first is that the id goes through a hash instead of being used directly. My ids come from `gen_random_uuid()`, so they are already random, and `id % interval` would spread them perfectly well. That is luck, not design. Reach for the top bytes instead of the whole value, or move to a time-ordered v7 id, and every monitor created in the same millisecond lands in the same slot. Hashing all sixteen bytes makes the offset independent of the id version and of which bits you happened to grab. `DefaultHasher` is SipHash, which is far more than this needs: the job is spreading load, not resisting an attacker.

The tempting alternative is to skip hashing and just count: hand the first monitor slot 0, the next one `interval / n`, and so on around the circle. It spreads perfectly, and it breaks the moment the fleet changes. Adding one monitor changes `n`, which moves every other monitor to a new slot. It also needs one place that knows the whole fleet at once, which is exactly the coordination the single-hash version does not need.

The difference only shows up once the fleet moves, so it is worth watching rather than reading:

<div class="mk-embed-stagger"></div>

*Each bar is one second of the interval, and its height is how many monitors are due in it. The fleet grows, shrinks and reloads in a different order underneath the rule, so read the running count of slot changes rather than the shape of any one frame.*

And hash the id, not the URL. It is tempting, because a URL carries more variety than a batch of ids. It is also wrong: two monitors watching the same URL would then get the same offset and fire together, which is the one collision you most want to avoid.

The offset also has to be stable. Restart the process and the same monitor lands in the same slot, because the input is only the id and the interval. Nothing is stored and no region has to coordinate with another, because each one computes the same answer on its own.

That is a claim worth checking, so the test bins a thousand ids and fails if any tenth of the interval is starved or crowded:

```rust
#[test]
fn stagger_offset_distributes_uniformly() {
    let base = Duration::from_secs(60);
    let bucket_ms = base.as_millis() as u64 / 10;
    let mut buckets = [0u32; 10];
    for n in 0..1000u128 {
        let o = stagger_offset(Uuid::from_u128(n + 0xDEAD_BEEF_0000), base);
        let idx = (o.as_millis() as u64 / bucket_ms).min(9) as usize;
        buckets[idx] += 1;
    }
    for (i, &c) in buckets.iter().enumerate() {
        assert!(
            (60..=160).contains(&c),
            "bucket {i} got {c} hits — distribution is clumped: {buckets:?}",
        );
    }
}
```

The first probe is a special case. Spreading a brand new monitor across its full interval is correct for load and terrible for the person who just created it and is staring at an empty chart. So the first one uses a much smaller window:

```rust
const INITIAL_PROBE_SPREAD: Duration = Duration::from_secs(5);

let cap = INITIAL_PROBE_SPREAD.min(base / 2);
```

Five seconds at most, and never more than half the interval, so a fast monitor cannot have its first probe pushed past its second one. After that first fire the entry switches to the full offset and settles onto its own grid.

## 4. Falling behind must not turn into a stampede

Something will eventually stall the loop. A slow batch, a paused container, a laptop lid. When it comes back, a naive scheduler sees a hundred missed slots and fires a hundred probes at once, which is the worst possible response to a machine that is already struggling.

Tokio's `Interval` has a name for the fix, `MissedTickBehavior::Skip`. The heap needs its own version:

```rust
/// First phase-aligned grid point strictly after `now` — the heap form of
/// `MissedTickBehavior::Skip`, dropping any backlog while preserving phase.
fn next_tick(sched: Instant, base: Duration, now: Instant) -> Instant {
    let next = sched + base;
    if next > now {
        return next;
    }
    let base_ns = base.as_nanos().max(1);
    let behind_ns = now.saturating_duration_since(next).as_nanos();
    let skips = ((behind_ns / base_ns) + 1).min(u32::MAX as u128) as u32;
    next + base * skips
}
```

The next time is computed from the scheduled time, not from `now`. That is the whole trick. Adding the interval to `now` after each run makes every run drift by however long the work took, and the drift accumulates forever. Adding to the previous scheduled point keeps the phase, so a check on a 20-second interval stays on the same 20-second grid all week.

When it is behind, it jumps forward whole intervals until it is in the future, and the missed ones are gone. For a monitor that is the right call. Nobody wants sixty stale probes replayed; they want the current state now.

```rust
#[tokio::test]
async fn next_tick_skips_backlog_when_behind() {
    let base = Duration::from_secs(10);
    let sched = Instant::now();
    let now = sched + base + base * 2 + base / 2;
    let next = next_tick(sched, base, now);
    assert!(next > now, "next {next:?} must be strictly future");
    assert_eq!(next, sched + base * 4);
}
```

## 5. Keep the slow thing off the driver

The config lives in Postgres. Something has to read it, notice new monitors and edits, and get them into the heap. The tempting place to put that is the driver loop, since that is where the heap is.

That would mean every scheduling decision waits behind a database round trip. A slow query, or a failover, and no check fires anywhere until it returns.

So the refresh runs on its own task and the driver only ever hears about the result:

```rust
// Refresh runs off the driver so a slow PG round-trip can never stall
// probe dispatch — the driver only ever does in-memory work.
let (diff_tx, mut diff_rx) = mpsc::channel::<RegistryDiff>(REFRESH_CHANNEL_BOUND);
let refresher = tokio::spawn(Arc::clone(&self).run_refresher(diff_tx, shutdown.clone()));
```

The channel is bounded at 8. If the driver ever falls behind, the refresher blocks on send instead of piling diffs up in memory, which turns a backlog into back pressure.

When the database is unreachable, the refresh backs off and the checks keep running on the last config it saw:

```rust
fn backoff_delay_secs(base_secs: u64, consecutive_failures: u32) -> u64 {
    let shift = consecutive_failures.saturating_sub(1).min(u32::BITS - 1);
    let mult = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let mult = mult.min(REFRESH_BACKOFF_CAP_MULTIPLIER);
    base_secs.saturating_mul(mult)
}
```

From a 30-second base the delay walks 30, 60, 120, 240, and then stops at 300 seconds. The cap matters as much as the doubling: uncapped backoff means that after a long outage the process waits hours to notice the database came back. Five minutes is the worst case here, and it is a number I picked on purpose rather than one that fell out of the formula.

The result is the property I was after. Postgres going away degrades the scheduler into "keeps doing what it was doing", not "stops".

## What carries over

Almost none of this is about uptime monitoring. Any system with recurring jobs on per-job intervals hits the same five walls:

- One owner for "what runs next", so the state needs no locks.
- Cancel by generation, not by removing from the queue.
- Deterministic hashed offsets, so a batch of jobs never lands on one instant.
- Compute the next time from the last scheduled time, and skip the backlog instead of replaying it.
- Keep I/O off the loop that makes the decisions, with a bounded channel and a capped backoff.

The whole scheduler is one file, about 460 lines including the tests, in `src/scheduler/runner.rs`.

Uptimepage is open source, AGPL-3.0: [github.com/uptimepage/uptimepage](https://github.com/uptimepage/uptimepage). The wider build story, one binary and two databases, is [here](https://uptimepage.dev/blog/building-an-uptime-monitor-in-rust); the HTTP client that runs on the other end of this dispatch is [here](https://uptimepage.dev/blog/http-prober-in-rust-no-reqwest); and where the check results go once they come back is [here](https://uptimepage.dev/blog/postgres-vs-clickhouse-uptime-monitor). To see where the scheduler sits in the whole system, there is a [live map of the architecture](/architecture).
