SET LOCAL lock_timeout = '5s';

UPDATE users SET terms_version = '' WHERE terms_version IS NULL;
UPDATE users SET privacy_version = '' WHERE privacy_version IS NULL;
ALTER TABLE users
    ALTER COLUMN terms_version SET NOT NULL,
    ALTER COLUMN privacy_version SET NOT NULL;
