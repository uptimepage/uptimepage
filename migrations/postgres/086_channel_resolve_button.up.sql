-- Whether this channel's alerts for an open incident carry a Resolve button.
-- Off until someone turns it on: closing an incident from a chat is a larger
-- step than taking it.
ALTER TABLE notification_channels
    ADD COLUMN resolve_button BOOLEAN NOT NULL DEFAULT false;
