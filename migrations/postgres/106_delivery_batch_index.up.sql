-- A stored batch is claimed and settled through the deliveries waiting for it.
CREATE INDEX idx_sp_deliveries_batch ON status_page_subscriber_deliveries (batch_key)
    WHERE batch_key IS NOT NULL;
