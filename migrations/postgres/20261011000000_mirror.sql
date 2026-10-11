-- Postgres keeps a copy of every node's books, members and loans (spec §6.2).
-- Same columns as SQLite, plus `node_id` (which node the row lives on) and
-- `synced_at`. Only the primary key is enforced: the node already enforces its
-- own rules, and re-checking them here would make batch order matter.

CREATE TABLE books (
    node_id        TEXT        NOT NULL,
    id             BIGINT      NOT NULL,
    title          TEXT        NOT NULL,
    author         TEXT        NOT NULL,
    isbn           TEXT,
    published_year BIGINT,
    total_copies   BIGINT      NOT NULL,
    created_at     BIGINT      NOT NULL,  -- Unix seconds, exactly as in SQLite
    updated_at     BIGINT      NOT NULL,
    synced_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

CREATE TABLE members (
    node_id    TEXT        NOT NULL,
    id         BIGINT      NOT NULL,
    name       TEXT        NOT NULL,
    email      TEXT        NOT NULL,
    created_at BIGINT      NOT NULL,
    updated_at BIGINT      NOT NULL,
    synced_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

CREATE TABLE loans (
    node_id     TEXT        NOT NULL,
    id          BIGINT      NOT NULL,
    book_id     BIGINT      NOT NULL,
    member_id   BIGINT      NOT NULL,
    borrowed_at BIGINT      NOT NULL,
    due_at      BIGINT      NOT NULL,
    returned_at BIGINT,
    synced_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

-- Where each node's copy stands (spec §6.4).
CREATE TABLE sync_checkpoints (
    node_id    TEXT        PRIMARY KEY,
    epoch      INTEGER     NOT NULL,  -- increases on every full copy
    last_seq   BIGINT      NOT NULL,  -- highest change_log seq applied here
    last_token BYTEA,                 -- token of that change_log row; NULL when last_seq = 0
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Rows replaced by a full copy (spec §6.5), kept for a person to inspect.
CREATE TABLE sync_archive (
    id          BIGSERIAL   PRIMARY KEY,
    node_id     TEXT        NOT NULL,
    epoch       INTEGER     NOT NULL,  -- the epoch these rows belonged to
    table_name  TEXT        NOT NULL,
    row_data    JSONB       NOT NULL,
    archived_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sync_archive_node_epoch ON sync_archive (node_id, epoch);
