-- The build an agent's config pulls report, as sent. NULL when the last pull
-- sent none.
ALTER TABLE agents ADD COLUMN build TEXT CHECK (build ~ '^[0-9A-Za-z._-]{1,64}$');
