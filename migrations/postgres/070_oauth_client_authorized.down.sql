DROP INDEX IF EXISTS idx_api_tokens_by_oauth_client;
DROP INDEX IF EXISTS idx_oauth_refresh_client;
DROP INDEX IF EXISTS idx_oauth_codes_client;
ALTER TABLE oauth_clients
    DROP COLUMN IF EXISTS last_authorized_at,
    DROP COLUMN IF EXISTS last_seen_at;
