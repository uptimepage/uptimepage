ALTER TABLE accounts DROP COLUMN IF EXISTS pending_interval, DROP COLUMN IF EXISTS billing_interval;
UPDATE plans SET is_listed = false, updated_at = now() WHERE id IN ('pro', 'team');
ALTER TABLE plan_prices
    DROP COLUMN IF EXISTS currency,
    DROP COLUMN IF EXISTS amount_minor;
