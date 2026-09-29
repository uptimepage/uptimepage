-- A named press stays the member's; one that named nobody has no older kind
-- that would not collide with the link acknowledgements already there. The
-- sender goes first: it is only allowed on an app's own kinds.
DELETE FROM incident_acknowledgements WHERE actor_type IN ('telegram', 'pushover', 'slack') AND anonymous;
DROP INDEX uq_incident_acknowledgements_anonymous;
ALTER TABLE incident_acknowledgements DROP COLUMN sender;
UPDATE incident_acknowledgements SET actor_type = 'user' WHERE actor_type IN ('telegram', 'pushover', 'slack');
CREATE UNIQUE INDEX uq_incident_acknowledgements_anonymous
    ON incident_acknowledgements (incident_id, episode, actor_type)
    WHERE anonymous;
ALTER TABLE incident_acknowledgements DROP CONSTRAINT incident_acknowledgements_actor_type_check;
ALTER TABLE incident_acknowledgements ADD CONSTRAINT incident_acknowledgements_actor_type_check
    CHECK (actor_type IN ('system','user','mcp','link'));
UPDATE incident_events
SET actor_type = CASE WHEN actor_id IS NULL THEN 'link' ELSE 'user' END
WHERE actor_type IN ('telegram', 'pushover', 'slack');
ALTER TABLE incident_events DROP CONSTRAINT incident_events_actor_type_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_actor_type_check
    CHECK (actor_type IN ('system','user','mcp','link'));
DROP TABLE IF EXISTS app_link_challenges;
DROP TABLE IF EXISTS linked_app_accounts;
