SET LOCAL lock_timeout = '5s';

-- Whether the platform wrote an update's words: detection and recovery notes,
-- and the default line a resolve or publish posts when the operator gave none.
-- The author cannot tell, since a resolve without a note is posted under the
-- person who resolved it. A listing leaves generated updates out and shows
-- what someone actually wrote.
ALTER TABLE incident_updates ADD COLUMN generated BOOLEAN NOT NULL DEFAULT false;

-- Rows from before the flag: the platform's own, a row with no author, or one
-- of the default lines it has always written.
UPDATE incident_updates SET generated = true
WHERE author = 'system'
   OR author IS NULL
   OR message IN (
       'Automatically detected — monitoring checks are failing.',
       'Automatically resolved — monitoring checks have recovered.',
       'This incident has been resolved.',
       'We are investigating this incident.',
       'Monitoring was removed; this incident was closed.'
   );
