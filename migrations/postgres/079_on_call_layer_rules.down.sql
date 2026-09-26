DROP INDEX IF EXISTS idx_on_call_layers_schedule;
CREATE INDEX idx_on_call_layers_schedule ON on_call_layers (org_id, schedule_id, layer_order);
ALTER TABLE on_call_layers DROP CONSTRAINT IF EXISTS ck_on_call_layers_custom_length;
