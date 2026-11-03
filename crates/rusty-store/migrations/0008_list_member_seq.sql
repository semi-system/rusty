-- Stable distribution-list member ordering without exposing SQLite's rowid.
-- Existing rows retain insertion order. New rows that omit seq (legacy callers and test
-- fixtures) receive the next per-list value in the AFTER INSERT trigger; SQLite serializes
-- writers, so two inserts cannot allocate the same value.
ALTER TABLE list_members ADD COLUMN seq INTEGER;

WITH ranked AS (
  SELECT rowid AS rid, row_number() OVER (PARTITION BY list_id ORDER BY rowid) AS n
  FROM list_members
)
UPDATE list_members
SET seq = (SELECT n FROM ranked WHERE ranked.rid = list_members.rowid);

CREATE UNIQUE INDEX list_members_seq ON list_members (list_id, seq);

CREATE TRIGGER list_members_seq_ins AFTER INSERT ON list_members
WHEN NEW.seq IS NULL
BEGIN
  UPDATE list_members
  SET seq = (
    SELECT COALESCE(MAX(seq), 0) + 1
    FROM list_members
    WHERE list_id = NEW.list_id AND rowid <> NEW.rowid
  )
  WHERE rowid = NEW.rowid;
END;

CREATE TRIGGER list_members_seq_upd BEFORE UPDATE OF seq ON list_members
WHEN NEW.seq IS NULL
BEGIN
  SELECT RAISE(ABORT, 'list member seq must not be null');
END;
