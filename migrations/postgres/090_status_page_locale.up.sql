ALTER TABLE status_pages
    ADD COLUMN public_locale TEXT NOT NULL DEFAULT 'en'
        CONSTRAINT status_page_locale_known CHECK (public_locale IN ('en', 'de'));
