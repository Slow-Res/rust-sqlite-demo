-- Indexes for the list and search queries (expand-only, S10: it adds indexes, a
-- virtual table and triggers, so the previous app version keeps working).
--
-- The ids are INTEGER PRIMARY KEYs (rowid aliases), so an index on (x) is already
-- ordered by (x, id). That is what `ORDER BY x, id` needs, and the page query reads
-- one slice of the index instead of sorting the whole table in a temp B-tree.
CREATE INDEX idx_books_title       ON books(title);
CREATE INDEX idx_members_name      ON members(name);
CREATE INDEX idx_loans_due         ON loans(due_at);  -- status=all; status=active has idx_loans_active_due
-- status=returned, scanned backwards for `returned_at DESC, id DESC`.
CREATE INDEX idx_loans_returned_at ON loans(returned_at) WHERE returned_at IS NOT NULL;

-- Book search. A trigram index answers `LIKE '%q%'` for q of 3+ characters.
-- External content: the index stores no copy of the text, it reads `books`.
CREATE VIRTUAL TABLE books_fts USING fts5(
    title, author, isbn,
    content = 'books', content_rowid = 'id',
    tokenize = 'trigram'
);
INSERT INTO books_fts(books_fts) VALUES ('rebuild');  -- index the books that already exist

-- Keep it in sync. These triggers have no WHEN condition: every write to `books`
-- fires them, whoever makes it. An external-content update first removes the old
-- entry with the 'delete' command and the OLD values, then adds the new one.
CREATE TRIGGER books_fts_insert AFTER INSERT ON books BEGIN
    INSERT INTO books_fts(rowid, title, author, isbn) VALUES (NEW.id, NEW.title, NEW.author, NEW.isbn);
END;
CREATE TRIGGER books_fts_delete AFTER DELETE ON books BEGIN
    INSERT INTO books_fts(books_fts, rowid, title, author, isbn) VALUES ('delete', OLD.id, OLD.title, OLD.author, OLD.isbn);
END;
CREATE TRIGGER books_fts_update AFTER UPDATE ON books BEGIN
    INSERT INTO books_fts(books_fts, rowid, title, author, isbn) VALUES ('delete', OLD.id, OLD.title, OLD.author, OLD.isbn);
    INSERT INTO books_fts(rowid, title, author, isbn) VALUES (NEW.id, NEW.title, NEW.author, NEW.isbn);
END;
