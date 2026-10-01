ALTER TABLE maintenance_windows
    DROP CONSTRAINT maintenance_windows_deleted_by_needs_deleted_at,
    DROP COLUMN deleted_by,
    DROP COLUMN deleted_at,
    DROP COLUMN updated_by,
    DROP COLUMN created_by;
