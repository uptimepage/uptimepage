SET LOCAL lock_timeout = '5s';

-- How long a recovered monitor must stay healthy before its incident closes.
-- A failure that confirms inside the window keeps the same incident open, so a
-- flapping service reads as one outage rather than a run of short ones. 0
-- closes on the confirming checks alone. The cap bounds how far back the
-- incident writer reads.
ALTER TABLE targets ADD COLUMN recovery_period_secs INTEGER NOT NULL DEFAULT 0
    CHECK (recovery_period_secs BETWEEN 0 AND 1800);

-- A monitor probed for availability holds three minutes unless it asks for
-- another hold, existing ones included. Heartbeat, manual and expiry monitors
-- close on the confirming checks alone: their next check can be hours away.
UPDATE targets SET recovery_period_secs = 180
 WHERE kind IN ('http', 'tcp', 'ping', 'dns', 'flow');

-- When the recovery an open incident is holding began. While it is set the
-- outage is over: paging pauses and downtime stops counting from then. A
-- failure that confirms inside the hold ends that recovery, and the stretch it
-- lasted is kept in the paired arrays so it never counts as downtime either.
ALTER TABLE incidents
    ADD COLUMN recovering_since TIMESTAMPTZ,
    ADD COLUMN recovered_from TIMESTAMPTZ[] NOT NULL DEFAULT '{}',
    ADD COLUMN recovered_until TIMESTAMPTZ[] NOT NULL DEFAULT '{}';
