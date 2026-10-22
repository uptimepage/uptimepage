-- The provider limits requests per second per team, not per process, so every
-- instance books its sends against this one row. `next_free_at` is the next
-- slot nobody holds; `blocked_until` is how long the provider last asked
-- everyone to wait after a 429.
CREATE TABLE email_send_gate (
    id            BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    next_free_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    blocked_until TIMESTAMPTZ NOT NULL DEFAULT '-infinity'
);

INSERT INTO email_send_gate DEFAULT VALUES;
