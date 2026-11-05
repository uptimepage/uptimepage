UPDATE status_pages SET public_locale = 'en' WHERE public_locale IN ('es', 'pt-PT');
ALTER TABLE status_pages DROP CONSTRAINT status_page_locale_known;
ALTER TABLE status_pages ADD CONSTRAINT status_page_locale_known
    CHECK (public_locale IN ('en', 'de'));
