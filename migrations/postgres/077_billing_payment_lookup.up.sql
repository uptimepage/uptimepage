-- A refund names only the transaction it reverses; this finds the account
-- that paid it once the subscription has been replaced.
CREATE INDEX idx_account_billing_events_payment_txn
    ON account_billing_events ((payload->>'transaction'))
    WHERE kind = 'payment_received';
