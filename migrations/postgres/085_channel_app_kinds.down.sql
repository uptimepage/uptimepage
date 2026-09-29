DELETE FROM notification_channels WHERE kind IN ('slack_app', 'discord_app');
ALTER TABLE notification_channels DROP CONSTRAINT notification_channels_kind_check;
ALTER TABLE notification_channels ADD CONSTRAINT notification_channels_kind_check
    CHECK (kind IN ('webhook', 'slack', 'telegram', 'telegram_app', 'whatsapp', 'whatsapp_app', 'discord', 'msteams', 'google_chat', 'email', 'pagerduty', 'ntfy', 'gotify', 'pushover', 'sms', 'mattermost'));
