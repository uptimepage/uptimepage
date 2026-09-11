-- Where the account lands when paid service ends. Recorded the first time the
-- plan moves, so a founding customer who buys a bigger tier and later stops
-- paying returns to founding, not to whatever the catalog hands out that day.
ALTER TABLE accounts ADD COLUMN fallback_plan_id TEXT REFERENCES plans(id);

-- Append-only record of what was done to an account's entitlements and why.
-- Org audit rows cannot carry it: a plan spans every org the account owns and
-- outlives any one of them.
CREATE TABLE account_billing_events (
    id         UUID PRIMARY KEY DEFAULT uuidv7(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    payload    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_account_billing_events_account
    ON account_billing_events(account_id, created_at);
