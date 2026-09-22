-- Verification permits certificate issuance; activation moves the URLs the
-- page publishes. Without the split, stamping verification would repoint
-- subscriber mail at a host that has never completed a TLS handshake.
ALTER TABLE status_pages ADD COLUMN custom_domain_activated_at TIMESTAMPTZ;

ALTER TABLE status_pages
    ADD CONSTRAINT status_pages_custom_domain_activation_after_verification
    CHECK (custom_domain_activated_at IS NULL OR custom_domain_verified_at IS NOT NULL);

-- Canonical form is what makes the existing UNIQUE index a real uniqueness
-- guarantee: CITEXT collides `Status.X` with `status.x`, but not `status.x.`
-- with `status.x`, nor an internationalised name with its punycode.
ALTER TABLE status_pages
    ADD CONSTRAINT status_pages_custom_domain_canonical
    CHECK (
        custom_domain IS NULL
        OR (
            char_length(custom_domain::text) <= 253
            AND custom_domain::text ~
                '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$'
        )
    );
