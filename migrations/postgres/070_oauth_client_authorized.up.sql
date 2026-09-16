-- Sweep keys for open registration: a client nobody approved and nobody has
-- opened the consent screen for in a week is dropped. Existing rows count as
-- approved only where a grant proves someone did approve; the rest are
-- crawler and scanner registrations and expire on the first sweep.
ALTER TABLE oauth_clients
    ADD COLUMN last_seen_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN last_authorized_at TIMESTAMPTZ;
UPDATE oauth_clients SET last_seen_at = created_at;
UPDATE oauth_clients c SET last_authorized_at = created_at
WHERE EXISTS (SELECT 1 FROM oauth_refresh_tokens r WHERE r.client_id = c.client_id)
   OR EXISTS (SELECT 1 FROM api_tokens t WHERE t.oauth_client_id = c.client_id);

CREATE INDEX idx_oauth_clients_unauthorized
    ON oauth_clients (last_seen_at)
    WHERE last_authorized_at IS NULL;

-- The sweep anti-joins on these. Codes and refresh tokens also cascade on
-- client delete; api_tokens.oauth_client_id carries no FK.
CREATE INDEX idx_oauth_codes_client ON oauth_authorization_codes (client_id);
CREATE INDEX idx_oauth_refresh_client ON oauth_refresh_tokens (client_id);
CREATE INDEX idx_api_tokens_by_oauth_client ON api_tokens (oauth_client_id)
    WHERE oauth_client_id IS NOT NULL;
