-- A custom rotation hands off at most once an hour: shorter is nothing a
-- person can staff, and the calendar walks every handoff of the month shown.
ALTER TABLE on_call_layers
    ADD CONSTRAINT ck_on_call_layers_custom_length
    CHECK (rotation_type <> 'custom' OR rotation_length_secs >= 3600);

-- No two layers of a schedule share an order, so which one pages is never
-- left to row order.
DROP INDEX idx_on_call_layers_schedule;
CREATE UNIQUE INDEX idx_on_call_layers_schedule
    ON on_call_layers (org_id, schedule_id, layer_order);
