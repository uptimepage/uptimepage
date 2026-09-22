ALTER TABLE status_pages DROP CONSTRAINT status_pages_custom_domain_canonical;
ALTER TABLE status_pages
    DROP CONSTRAINT status_pages_custom_domain_activation_after_verification;
ALTER TABLE status_pages DROP COLUMN custom_domain_activated_at;
