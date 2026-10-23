+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "Postgres vs ClickHouse is the wrong question. I use both."
date = "2026-07-08"
updated = "2026-08-03"
slug = "postgres-vs-clickhouse-uptime-monitor"
excerpt = "My uptime monitor runs Postgres and ClickHouse side by side. Which data goes where comes down to one question, and four database tricks fall out of it."
tags = ["postgres", "clickhouse", "databases", "rust", "monitoring"]
draft = false
+++

> **TL;DR.** My uptime monitor keeps config and incidents in Postgres and check results in ClickHouse. The split is one rule: does a row ever change? Config gets edited, so it wants Postgres transactions and constraints. A check result is written once and never touched again, so it goes to ClickHouse, where the right codec, a careful sort key, and a per-row TTL make billions of rows cheap. Four tricks and a bonus below, useful even if you only ever run one database.

Every monitor I run writes a row every 20 seconds, from every region, and never stops. One monitor is about 4,300 rows a day, per region. Multiply by every monitor on the platform and the rows only ever go up.

That stream would slowly crush a normal Postgres table, and watching it is the whole product. So the check results do not live in Postgres. They live in ClickHouse. Everything else, the monitors and incidents and teams, lives in Postgres. The interesting part is the line between them.

People ask why not one database. "Postgres vs ClickHouse" is the wrong question, because the two are not fighting over the same job. Here is the line I draw, and the one rule under all of it: split your data by how it is written, not by which engine is faster.

## 1. Does the row change? That picks the database

Forget benchmarks for a second. One question sorts a table into one store or the other: after you write a row, will you ever change it?

```text
one monitor, two stores:

  you edit a monitor  ->  Postgres   (targets, incidents, team)
                          the row changes, has constraints, lives in a transaction

  a probe checks it   ->  ClickHouse (check_results)
                          one row, written once, never updated
```

A monitor is a row that changes. You toggle it off, edit the URL, change the interval. So it lives in Postgres:

```sql
CREATE TABLE targets (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL,
    check_spec    JSONB NOT NULL,
    interval_secs INTEGER NOT NULL CHECK (interval_secs >= 10),
    enabled       BOOLEAN NOT NULL DEFAULT true,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

That `CHECK` means a 3-second interval can never reach the table, no matter which part of the app tried to write it. This is Postgres doing the thing it is best at: a small set of rows that must stay correct while many callers change them at once.

A check result is the opposite. It is written once when a probe finishes, and then it never changes. Nothing ever updates it, so it does not need any of that:

```sql
CREATE TABLE check_results (
    org_id      UUID,
    target_id   UUID,
    region      LowCardinality(String),
    timestamp   DateTime('UTC') CODEC(DoubleDelta, ZSTD(1)),
    status      Enum8('up' = 1, 'down' = 2, 'degraded' = 3, 'error' = 4),
    duration_ms UInt32 CODEC(T64, ZSTD(1))
) ENGINE = MergeTree
```

No `updated_at`, no foreign key, no transaction. Just an append-only stream that grows forever. That is the shape ClickHouse is built for and the shape that slowly hurts Postgres.

Notice the types too. `status` is an `Enum8`, one byte on disk, not the string `"up"`. `region` is `LowCardinality`, stored once in a dictionary and referenced by a small id instead of repeating the text on every row. On a table that only ever grows, a byte saved per row is a byte saved times billions.

## 2. The right codec turns a day of timestamps into almost nothing

Once the results are in a column store, the type on each column is most of the compression, and the default setting wastes a lot of space.

Look at that timestamp again. A monitor checks every 20 seconds, so for one monitor a day of timestamps is a run of 20, 20, 20, 20, over and over. `DoubleDelta` stores the change in the change. For a steady interval the first delta is a constant 20 and the second delta is zero, so each row after the first packs down to about a bit. The codec is not a ClickHouse invention: it comes from Facebook's Gorilla time-series paper, built for exactly this, measurements taken at a steady rate.

```sql
timestamp   DateTime('UTC')  CODEC(DoubleDelta, ZSTD(1)),
duration_ms UInt32           CODEC(T64, ZSTD(1)),
dns_ms      Nullable(UInt16) CODEC(T64, ZSTD(1)),
```

I also store the timestamp at whole seconds on purpose. The smallest interval is 20 seconds, so no two checks for one monitor ever land in the same second, and the sub-second detail lives in `duration_ms` where it belongs. Whole seconds that step by a fixed amount compress far harder than milliseconds would.

The latency columns get `T64` instead, because they are small integers that stay in a small range, a different shape from a steady clock. `T64` crops the unused high bits off a block of values, so a `UInt32` that never climbs past a few thousand milliseconds stops paying for all 32 bits. That is the trick worth copying, and it is not the specific codec names. It is that a timestamp ticking by a fixed step and a latency staying in a small range are two different shapes, and telling the database which is which does the work.

## 3. Put the tenant in the sort key, not the partition

I got this one wrong first, so learn it from my mistake instead of your own.

This is a multi-tenant app, so every row carries an `org_id` and almost every query filters by it. My first schema partitioned by org, one slot per customer, because it felt tidy. ClickHouse turned that into a flood of tiny parts, the merges could not keep up, and startup got slower the more customers I had. I ripped it out. A partition is not a folder for tidiness. It is a physical unit ClickHouse merges and expires, and you want few large ones, not many small ones.

Here is what it should be:

```sql
) ENGINE = MergeTree
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (org_id, target_id, region, timestamp)
```

`org_id` leads the `ORDER BY`, so a per-org query walks the sorted primary index and reads only that org's slice. But it stays out of `PARTITION BY`. The rule I follow now: partition by something low-cardinality that you also delete by, here the day, and put the high-cardinality tenant key in the sort order. Sort key answers "find this org fast". Partition answers "drop old data cheaply", which is the next trick.

## 4. Make retention a number in a row, not a schema change

Different plans keep history for different lengths of time. The clumsy way is a migration or a cleanup job per plan. The clean way is to make the retention window a column:

```sql
ttl_days UInt16 DEFAULT 30 CODEC(ZSTD(1))
) ENGINE = MergeTree
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (org_id, target_id, region, timestamp)
TTL timestamp + toIntervalDay(ttl_days)
```

Each row carries its own `ttl_days`, stamped from the org's plan at the moment it is written. A free plan keeps 30 days, a paid plan keeps more, and changing that needs no `ALTER`, no migration, no backfill. The next write just stores a different number. The daily partitions line up with the TTL, so ClickHouse expires old data by dropping whole parts, close to free, and a whole column of the same `30` compresses away to nothing. The flexibility costs no space.

One more piece makes reading that history cheap. A materialized view rolls raw checks into per-minute and per-hour summaries as they land:

```sql
CREATE MATERIALIZED VIEW check_results_1m
ENGINE = AggregatingMergeTree
ORDER BY (org_id, target_id, region, minute)
AS SELECT
    org_id, target_id, region,
    toStartOfMinute(timestamp)  AS minute,
    countState()                AS total_checks,
    countIfState(status = 'up') AS up_checks,
    avgState(duration_ms)       AS avg_duration_ms
FROM check_results
GROUP BY org_id, target_id, region, minute;
```

So a dashboard showing 30 days across a thousand monitors reads a few thousand minute-buckets, not millions of raw rows. Recent views read the minute rollup, long history reads an hour rollup, and the raw rows underneath expire on their own TTL. The firehose is there when you need to drill into one bad minute, and left alone the rest of the time.

## One more, because everyone hits it: make inserts idempotent

Here is the trick I wish someone had told me first. My agents batch check results and send them to ClickHouse. A network blip after the server commits but before my side gets the ack means the batch retries and sends the exact same block again. Without protection, that double-counts every row in it, and your uptime numbers quietly drift.

ClickHouse has a fix built in, but for a plain (non-Replicated) MergeTree it is off until you turn it on:

```sql
SETTINGS index_granularity = 8192,
         non_replicated_deduplication_window = 1000;
```

The server hashes each inserted block and remembers the last 1,000 hashes. A retry sends an identical block, the hash matches, and the server drops it instead of appending it again. The retry becomes safe to do blindly, so the client stays simple: send, and if unsure, send again. Make the window bigger than the most blocks a single retry could resend, and you stop having to worry about duplicate writes.

## So, one database or two?

For almost every app, one. If your data fits in Postgres and stays fast, a second database is a cost you should not pay: two schemas, two clients, two things to back up and think about.

Reach for the second store only when one table stops looking like the rest of your tables. For me that table was `check_results`. It is written once and never edited, it grows without end, and every question I ask it is a summary over a time range. That is a different shape from my monitors and my incidents, so it wanted a different database.

The rule I would give my past self: do not split by "which database is faster". Split by how the data is written. Rows that change and must stay correct want Postgres. An append-only stream you only ever summarize wants a column store like ClickHouse. Most of the tricks above are that one idea pushed down into the schema.

Uptimepage is open source, AGPL-3.0, and both schemas are in the repo: [github.com/uptimepage/uptimepage](https://github.com/uptimepage/uptimepage). The probe that writes those rows is [its own post](https://uptimepage.dev/blog/http-prober-in-rust-no-reqwest), and the wider build story, one binary and two databases, is [here](https://uptimepage.dev/blog/building-an-uptime-monitor-in-rust). To see where both stores sit in the whole system, and which path a check result takes to reach them, there is a [live map of the architecture](/architecture). Or [start free on the hosted tier](https://uptimepage.dev) and point a check at something.
