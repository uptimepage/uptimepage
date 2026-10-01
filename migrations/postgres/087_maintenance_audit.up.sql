ALTER TABLE maintenance_windows
    ADD COLUMN created_by UUID REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN updated_by UUID REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD COLUMN deleted_by UUID REFERENCES users(id) ON DELETE SET NULL,
    ADD CONSTRAINT maintenance_windows_deleted_by_needs_deleted_at
        CHECK (deleted_by IS NULL OR deleted_at IS NOT NULL);
