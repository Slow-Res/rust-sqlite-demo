-- Book search by word prefix instead of substring (S10: derived data only, so
-- the previous app version keeps working; its trigram queries just match words now).
--
-- The trigram index served `LIKE '%q%'` and had to read every posting of a common
-- word. The word index below stores whole words plus their 2- and 3-character
-- prefixes (`prefix = '2 3'`), so `"anc"*` is one lookup in the prefix list and
-- longer prefixes scan a short range of the word list. It also works from 1 character.
-- `remove_diacritics 0` keeps accents significant ("é" is not "e"), as before.
-- External content: the index stores no copy of the text, it reads `books`.
DROP TRIGGER books_fts_insert;
DROP TRIGGER books_fts_delete;
DROP TRIGGER books_fts_update;
DROP TABLE books_fts;

CREATE VIRTUAL TABLE books_fts USING fts5(
    title, author, isbn,
    content = 'books', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 0',
    prefix = '2 3'
);
INSERT INTO books_fts(books_fts) VALUES ('rebuild');  -- index the books that already exist

-- Same triggers as before (see 20261012000000_perf_indexes.sql).
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
