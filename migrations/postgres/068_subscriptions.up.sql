-- The paid side of an account: where the subscription stands with the payment
-- provider, and the two clocks the lifecycle runs on. Every column is named in
-- our terms; the provider is a value in `billing_provider`, so a second one is
-- a new row in `plan_prices`, not a new column.

ALTER TABLE accounts
    ADD COLUMN subscription_status TEXT NOT NULL DEFAULT 'none'
        CHECK (subscription_status IN ('none', 'active', 'past_due', 'canceled')),
    -- A scheduled move: a voluntary downgrade or a cancel that takes effect
    -- when the paid period ends. The customer keeps the plan until then.
    ADD COLUMN pending_plan_id TEXT REFERENCES plans(id),
    ADD COLUMN plan_change_at TIMESTAMPTZ,
    -- A cancel booked with the provider: paid service ends here and the
    -- account lands on its fallback. Kept apart from a scheduled move, since
    -- a fallback that is itself on sale would make the two look alike.
    ADD COLUMN cancel_at TIMESTAMPTZ,
    ADD COLUMN current_period_end TIMESTAMPTZ,
    -- Full service continues until here after a failed payment. Set by the
    -- first failure and never moved by a later one, so retries cannot extend it.
    ADD COLUMN grace_until TIMESTAMPTZ,
    -- Reminders already sent inside the current grace window.
    ADD COLUMN dunning_stage SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN billing_provider TEXT,
    ADD COLUMN provider_customer_ref TEXT,
    ADD COLUMN provider_subscription_ref TEXT,
    -- When the provider's last applied snapshot was taken. Webhooks arrive in
    -- any order; one older than this is dropped rather than applied.
    ADD COLUMN subscription_synced_at TIMESTAMPTZ,
    -- The same for payment events, which carry no snapshot and so keep their
    -- own watermark.
    ADD COLUMN payment_synced_at TIMESTAMPTZ;

CREATE UNIQUE INDEX idx_accounts_provider_subscription
    ON accounts(billing_provider, provider_subscription_ref)
    WHERE provider_subscription_ref IS NOT NULL;

-- The lifecycle sweep reads only accounts with a deadline set.
CREATE INDEX idx_accounts_plan_change_due
    ON accounts(plan_change_at) WHERE plan_change_at IS NOT NULL;
CREATE INDEX idx_accounts_cancel_due
    ON accounts(cancel_at) WHERE cancel_at IS NOT NULL;
CREATE INDEX idx_accounts_grace_due
    ON accounts(grace_until) WHERE grace_until IS NOT NULL;

-- Which provider price sells which plan on which cadence. Data rather than
-- code so a new price, or a whole new provider, is an INSERT.
CREATE TABLE plan_prices (
    provider   TEXT NOT NULL,
    price_ref  TEXT NOT NULL,
    plan_id    TEXT NOT NULL REFERENCES plans(id),
    interval   TEXT NOT NULL CHECK (interval IN ('month', 'year')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, price_ref),
    UNIQUE (provider, plan_id, interval)
);

-- Every provider event acted on, keyed by the provider's own id, so a redelivery
-- is acknowledged without being applied twice.
CREATE TABLE provider_events (
    provider    TEXT NOT NULL,
    event_id    TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, event_id)
);
