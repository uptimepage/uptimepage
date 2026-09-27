-- A member's calendar feed of their own on-call shifts. The link carries a
-- secret token: the feed is looked up by its SHA-256, and the sealed copy lets
-- the page show the link again. It goes with the membership.
CREATE TABLE on_call_feeds (
    user_id    UUID NOT NULL,
    org_id     UUID NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    token_enc  TEXT NOT NULL,
    PRIMARY KEY (user_id, org_id),
    FOREIGN KEY (user_id, org_id) REFERENCES memberships (user_id, org_id) ON DELETE CASCADE
);
