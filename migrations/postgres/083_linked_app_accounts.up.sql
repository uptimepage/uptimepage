-- Accounts in other apps a person proved are theirs, so an acknowledgement
-- pressed there can name them. Held per user rather than per org: the org a
-- press lands in still has to count them as a member.
CREATE TABLE linked_app_accounts (
    id            UUID PRIMARY KEY DEFAULT uuidv7(),
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app           TEXT NOT NULL CONSTRAINT linked_app_accounts_app_check
                      CHECK (app IN ('telegram','pushover')),
    -- Keyed hash of the app's id for the person. Only ever matched, and a
    -- Pushover user key is as good as a phone number.
    external_hash TEXT NOT NULL,
    label         TEXT CHECK (char_length(label) <= 128),
    linked_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One person per app account. Linking one held by someone else is refused
-- until they unlink it, never moved.
CREATE UNIQUE INDEX uq_linked_app_accounts_external
    ON linked_app_accounts (app, external_hash);
CREATE INDEX idx_linked_app_accounts_user ON linked_app_accounts (user_id);

-- A single-use offer to link. A Telegram one names the person who asked for it,
-- and the Telegram account that presses Start with it becomes theirs. A
-- Pushover one names the account it was pushed to, which becomes whoever signs
-- in to open it. Spent in the transaction that writes the link.
CREATE TABLE app_link_challenges (
    id            UUID PRIMARY KEY DEFAULT uuidv7(),
    code_hash     TEXT NOT NULL UNIQUE,
    app           TEXT NOT NULL CONSTRAINT app_link_challenges_app_check
                      CHECK (app IN ('telegram','pushover')),
    user_id       UUID REFERENCES users(id) ON DELETE CASCADE,
    external_hash TEXT,
    label         TEXT CHECK (char_length(label) <= 128),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    consumed_at   TIMESTAMPTZ,
    CHECK ((user_id IS NULL) <> (external_hash IS NULL))
);

-- Serves the offer cooldown and voiding a person's earlier Telegram codes.
CREATE INDEX idx_app_link_challenges_account
    ON app_link_challenges (app, external_hash, created_at)
    WHERE external_hash IS NOT NULL;
CREATE INDEX idx_app_link_challenges_user
    ON app_link_challenges (user_id)
    WHERE user_id IS NOT NULL;
-- Serves the daily retention purge.
CREATE INDEX idx_app_link_challenges_created ON app_link_challenges (created_at);

-- Acknowledgements pressed in those apps. One names the member who linked the
-- account, so it shares the member's row per episode with the web and MCP.
ALTER TABLE incident_events DROP CONSTRAINT incident_events_actor_type_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_actor_type_check
    CHECK (actor_type IN ('system','user','mcp','link','telegram','pushover'));
ALTER TABLE incident_acknowledgements DROP CONSTRAINT incident_acknowledgements_actor_type_check;
ALTER TABLE incident_acknowledgements ADD CONSTRAINT incident_acknowledgements_actor_type_check
    CHECK (actor_type IN ('system','user','mcp','link','telegram','pushover'));

-- Who pressed, as the app knows them, kept only when nobody is named: two
-- people nobody linked are still two acknowledgements. A signed link knows no
-- sender, so it keeps counting once.
ALTER TABLE incident_acknowledgements
    ADD COLUMN sender TEXT CHECK (sender IS NULL OR (anonymous AND actor_type IN ('telegram','pushover')));
DROP INDEX uq_incident_acknowledgements_anonymous;
CREATE UNIQUE INDEX uq_incident_acknowledgements_anonymous
    ON incident_acknowledgements (incident_id, episode, actor_type, sender) NULLS NOT DISTINCT
    WHERE anonymous;
