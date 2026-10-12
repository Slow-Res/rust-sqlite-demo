-- Full indexes on the loans foreign keys.
--
-- `loans.book_id` and `loans.member_id` are `REFERENCES ... ON DELETE CASCADE`.
-- Deleting a book (or member) looks up all its loans, returned ones too, and the
-- only indexes on these columns are partial (`WHERE returned_at IS NULL`), which
-- cannot serve that lookup: every delete scanned the whole `loans` table. The
-- cascade needs an index on the referencing column that covers every row.
-- The partial active indexes stay: the active-loan counts and the availability
-- count read them, and they are smaller.
CREATE INDEX idx_loans_book   ON loans(book_id);
CREATE INDEX idx_loans_member ON loans(member_id);
