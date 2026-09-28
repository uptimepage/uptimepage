-- Whether this channel's alerts for an open incident carry an Acknowledge
-- button. Off for a room whose readers should not take incidents from it.
ALTER TABLE notification_channels
    ADD COLUMN acknowledge_button BOOLEAN NOT NULL DEFAULT true;
