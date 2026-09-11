DROP TABLE IF EXISTS account_billing_events;
ALTER TABLE accounts DROP COLUMN IF EXISTS fallback_plan_id;
