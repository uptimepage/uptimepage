ALTER TABLE status_pages
    ADD COLUMN public_timezone TEXT NOT NULL DEFAULT 'UTC';
