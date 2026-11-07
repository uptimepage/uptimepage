-- What each alert source sent and what it did: the source's delivery log.
-- History only, never state, so there is no Postgres row per event.

CREATE TABLE IF NOT EXISTS alert_events (
    org_id        UUID,
    source_id     UUID,
    -- Every event of one request shares it, so the request reads as a group.
    received_at   DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)),
    ingested_at   DateTime('UTC') DEFAULT now() CODEC(DoubleDelta, ZSTD(1)),
    outcome       Enum8('opened' = 1, 'repeat' = 2, 'resolved' = 3, 'ignored' = 4,
                        'paused' = 5, 'held' = 6, 'capped' = 7, 'unparsed' = 8,
                        'unavailable' = 9),
    -- As stored on the incident; empty for a request that carried no event.
    dedup_key     String CODEC(ZSTD(1)),
    incident_id   Nullable(UUID),
    -- Why it did what it did, when the outcome alone does not say.
    note          String CODEC(ZSTD(1)),
    -- The start of the request's body, on its first event only, and empty
    -- when every event was a repeat. The per-column TTL drops it ahead of
    -- the row.
    body          String CODEC(ZSTD(1)) TTL toDateTime(received_at) + toIntervalDay(evidence_days),
    -- Stamped from the org's plan at write time, so a policy change needs no DDL.
    evidence_days UInt16 DEFAULT 7 CODEC(ZSTD(1)),
    ttl_days      UInt16 DEFAULT 30 CODEC(ZSTD(1))
) ENGINE = MergeTree
PARTITION BY toYYYYMM(received_at)
ORDER BY (org_id, source_id, received_at)
TTL toDateTime(received_at) + toIntervalDay(ttl_days)
-- The writer re-sends the identical block on retry.
SETTINGS index_granularity = 8192, non_replicated_deduplication_window = 1000;
