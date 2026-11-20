-- What an agent's config pulls say it holds. NULL config_missing_since: the
-- agent showed a config and has been in touch since. scheduled_monitors: the
-- count it last reported, NULL when it holds none or did not say.
ALTER TABLE agents
    ADD COLUMN config_missing_since TIMESTAMPTZ,
    ADD COLUMN scheduled_monitors   INTEGER CHECK (scheduled_monitors >= 0);
