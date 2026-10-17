-- The two tables the hourly retention passes delete from (the journal's
-- order-book updates, the recorded market history) are vacuumed once 2 % of
-- their rows are dead rather than PostgreSQL's 20 %. The space of deleted
-- rows is reusable only after a vacuum: at 20 % the journal carried up to
-- about a day and a half of deleted book updates, room the files keep.
ALTER TABLE event_journal SET (autovacuum_vacuum_scale_factor = 0.02);
ALTER TABLE orderbook_snapshots SET (autovacuum_vacuum_scale_factor = 0.02);
