-- A minute bucket held about one check per monitor and region, so the view
-- kept as many rows as the raw table at four times its size, and readers
-- moved to raw results and the hour rollup. SYNC frees its parts now rather
-- than after the server's drop delay.
DROP VIEW IF EXISTS check_results_1m SYNC;
