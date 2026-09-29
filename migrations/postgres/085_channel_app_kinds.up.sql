-- Slack and Discord channels connected through our own apps, whose button
-- presses reach us.
ALTER TABLE notification_channels DROP CONSTRAINT notification_channels_kind_check;
ALTER TABLE notification_channels ADD CONSTRAINT notification_channels_kind_check
    CHECK (kind IN ('webhook', 'slack', 'slack_app', 'telegram', 'telegram_app', 'whatsapp', 'whatsapp_app', 'discord', 'discord_app', 'msteams', 'google_chat', 'email', 'pagerduty', 'ntfy', 'gotify', 'pushover', 'sms', 'mattermost'));
