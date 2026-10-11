-- Book search by word prefix instead of substring, the same meaning as the
-- SQLite FTS5 index: every query word is a prefix of some word of title, author
-- or ISBN. One GIN index on the document replaces the three trigram indexes (and
-- pg_trgm), which had to read every posting of a common word.
--
-- The default text-search parser keeps "J.R.R." and "foo-bar" as single tokens,
-- so the punctuation of ASCII is turned into spaces first: words are then runs
-- of letters and digits, like the Rust side splits the query (db::search_words).
-- Non-ASCII characters are left to the parser. An IMMUTABLE function, so the
-- index and the queries share one definition.
CREATE FUNCTION book_search_vector(title text, author text, isbn text) RETURNS tsvector
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
    SELECT to_tsvector('simple', regexp_replace(
        title || ' ' || author || ' ' || coalesce(isbn, ''),
        '[^0-9A-Za-z\u0080-\U0010FFFF]+', ' ', 'g'))
$$;

CREATE INDEX idx_books_search ON books USING gin (book_search_vector(title, author, isbn));

DROP INDEX idx_books_title_trgm;
DROP INDEX idx_books_author_trgm;
DROP INDEX idx_books_isbn_trgm;
DROP EXTENSION pg_trgm;
