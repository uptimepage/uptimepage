SET LOCAL lock_timeout = '5s';

-- How long a recovered monitor must stay healthy before its incident closes.
-- A failure that confirms inside the window keeps the same incident open, so a
-- flapping service reads as one outage rather than a run of short ones. 0
-- closes on the confirming checks alone, which is how every monitor behaved
-- before. The cap bounds how far back the incident writer reads.
ALTER TABLE targets ADD COLUMN recovery_period_secs INTEGER NOT NULL DEFAULT 0
    CHECK (recovery_period_secs BETWEEN 0 AND 1800);
