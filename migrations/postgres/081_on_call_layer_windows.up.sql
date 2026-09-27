-- When a layer is on call: a list of weekly windows in the schedule's
-- timezone, each some weekdays from one time of day to another. Empty means
-- at all hours.
ALTER TABLE on_call_layers
    ADD COLUMN windows JSONB NOT NULL DEFAULT '[]'
        CONSTRAINT ck_on_call_layers_windows CHECK (jsonb_typeof(windows) = 'array');
