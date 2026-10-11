CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE sessions (
    token_hash TEXT    PRIMARY KEY,          -- hex(SHA-256(raw token)); raw token only in the cookie
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);

CREATE TABLE books (
    id             INTEGER PRIMARY KEY,
    title          TEXT    NOT NULL,
    author         TEXT    NOT NULL,
    isbn           TEXT,                      -- normalized digits (+ 'X'), NULL if unknown
    published_year INTEGER,
    total_copies   INTEGER NOT NULL CHECK (total_copies BETWEEN 1 AND 1000),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_books_isbn ON books(isbn) WHERE isbn IS NOT NULL;

CREATE TABLE members (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    email      TEXT    NOT NULL UNIQUE,      -- stored lowercased
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE loans (
    id          INTEGER PRIMARY KEY,
    book_id     INTEGER NOT NULL REFERENCES books(id)   ON DELETE CASCADE,
    member_id   INTEGER NOT NULL REFERENCES members(id) ON DELETE CASCADE,
    borrowed_at INTEGER NOT NULL,
    due_at      INTEGER NOT NULL,
    returned_at INTEGER
);
CREATE INDEX idx_loans_active_book   ON loans(book_id)   WHERE returned_at IS NULL;
CREATE INDEX idx_loans_active_member ON loans(member_id) WHERE returned_at IS NULL;
CREATE INDEX idx_loans_active_due    ON loans(due_at)    WHERE returned_at IS NULL;
