-- What a price costs, so the billing page can say so without asking the
-- provider. Minor units and an ISO code, as the provider quotes them.
ALTER TABLE plan_prices
    ADD COLUMN amount_minor INTEGER NOT NULL CHECK (amount_minor >= 0),
    ADD COLUMN currency     TEXT    NOT NULL CHECK (currency ~ '^[A-Z]{3}$');

UPDATE plans SET is_listed = true, updated_at = now() WHERE id IN ('pro', 'team');

-- The cadence a live subscription bills on, from the price it resolved
-- through, so the billing page can offer the other one.
ALTER TABLE accounts
    ADD COLUMN billing_interval TEXT CHECK (billing_interval IN ('month', 'year'));
