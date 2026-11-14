-- The incident writer asks every pass for what has landed since its last one.
-- A day's part is sorted by monitor, not by arrival, so without this it reads
-- a granule per monitor in every part of the window to find nothing new.
ALTER TABLE check_results
    ADD INDEX IF NOT EXISTS ingested_at_minmax ingested_at TYPE minmax GRANULARITY 1;

-- Parts written before the index carry none and are read as before. Building
-- it for them is a background mutation over one column.
ALTER TABLE check_results MATERIALIZE INDEX IF EXISTS ingested_at_minmax;
