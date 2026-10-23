SET LOCAL lock_timeout = '5s';

-- NULL: no such policy was shown when the account was created.
ALTER TABLE users
    ALTER COLUMN terms_version DROP NOT NULL,
    ALTER COLUMN privacy_version DROP NOT NULL;
