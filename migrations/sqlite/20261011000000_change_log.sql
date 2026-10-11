-- The change log (spec §6.1). Triggers add one row for every insert, update or
-- delete on a synced table, inside the same transaction as the write, so a
-- change is logged if and only if it is committed. The sync worker reads this
-- table and copies the changed rows to Postgres.
--
-- The log says WHICH row changed, not what it contains: the worker reads the
-- row's current values when it ships them. Adding a column to a synced table
-- therefore needs no trigger change.
CREATE TABLE change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- AUTOINCREMENT: a number is never reused
    table_name TEXT    NOT NULL,
    row_id     INTEGER NOT NULL,
    op         TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')),  -- for people reading the log
    token      BLOB    NOT NULL DEFAULT (randomblob(8)),  -- random fingerprint that detects a restored database (spec §6.4)
    created_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER))  -- Unix ms, for lag
);

CREATE TRIGGER books_logged_insert AFTER INSERT ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'insert');
END;
CREATE TRIGGER books_logged_update AFTER UPDATE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'update');
    -- If the id itself changed, the row under the old id is gone.
    INSERT INTO change_log (table_name, row_id, op) SELECT 'books', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER books_logged_delete AFTER DELETE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', OLD.id, 'delete');
END;

CREATE TRIGGER members_logged_insert AFTER INSERT ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', NEW.id, 'insert');
END;
CREATE TRIGGER members_logged_update AFTER UPDATE ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', NEW.id, 'update');
    INSERT INTO change_log (table_name, row_id, op) SELECT 'members', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER members_logged_delete AFTER DELETE ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', OLD.id, 'delete');
END;

-- Cascaded deletes (deleting a book or member deletes its loans) fire these too.
CREATE TRIGGER loans_logged_insert AFTER INSERT ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', NEW.id, 'insert');
END;
CREATE TRIGGER loans_logged_update AFTER UPDATE ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', NEW.id, 'update');
    INSERT INTO change_log (table_name, row_id, op) SELECT 'loans', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER loans_logged_delete AFTER DELETE ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', OLD.id, 'delete');
END;
