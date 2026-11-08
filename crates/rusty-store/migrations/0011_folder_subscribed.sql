-- IMAP SUBSCRIBE / LSUB state of a folder (U6.0). One flag per folder; the five system folders
-- (created by create_account without naming the column) start subscribed. A folder created
-- through Mailbox::create_folder starts unsubscribed (the statement sets it); folders that
-- exist today stay subscribed so nothing disappears from a client's folder list.
--
-- PostgreSQL twin: migrations_pg/0002_folder_subscribed.sql (SQLite 0011 -> PG 0002; SQLite 0010
-- is reserved for U2 and has no PG number yet).
ALTER TABLE folders ADD COLUMN subscribed INTEGER NOT NULL DEFAULT 1 CHECK (subscribed IN (0, 1));
