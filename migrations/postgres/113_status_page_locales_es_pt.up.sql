ALTER TABLE status_pages DROP CONSTRAINT status_page_locale_known;
ALTER TABLE status_pages ADD CONSTRAINT status_page_locale_known
    CHECK (public_locale IN ('en', 'de', 'es', 'pt-PT'));
