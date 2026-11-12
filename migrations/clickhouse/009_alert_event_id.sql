-- Orders events that share an arrival time and a key, so a page of the
-- delivery log can end between them. Rows written before it all read as zero.
ALTER TABLE alert_events
    ADD COLUMN IF NOT EXISTS event_id UUID AFTER dedup_key;
