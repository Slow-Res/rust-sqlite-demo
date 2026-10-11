-- Leave 10% of every page free so updates can be HOT (heap-only tuples): the
-- load test updates book and member rows constantly (loans change the counts,
-- edits change the rows), and when the new row version fits in the same page
-- Postgres adds no new index entries. The indexes then hold no dead entries for
-- those updates, and index scans need fewer heap visits.
-- It applies to new pages only; pages that already exist keep their fill until
-- the table is rewritten (VACUUM FULL or CLUSTER).
ALTER TABLE books   SET (fillfactor = 90);
ALTER TABLE members SET (fillfactor = 90);
ALTER TABLE loans   SET (fillfactor = 90);
