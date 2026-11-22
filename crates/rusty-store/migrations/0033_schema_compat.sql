-- Rolling updates without downtime (docs/design-zero-downtime.md): during an update the old and the new release run side by side on the SAME schema, and an old task that restarts in that
-- window must still start. So a binary no longer insists on the schema being exactly its own: it also runs on a NEWER schema when every migration it does not know says so here.
--
-- One row per migration from this one on: `min_binary` is the schema version (the number of the last migration) of the OLDEST binary that keeps working once this migration is applied. An
-- additive migration (a new table, a new nullable column, a new index) says the version just before it; a migration that an older binary cannot live with says its own version, which is what
-- a migration with no row means too (the safe default: a migration written without thinking about it stops old binaries, as before).
CREATE TABLE schema_compat (
  version    bigint PRIMARY KEY CONSTRAINT schema_compat_version_valid CHECK (version >= 1),
  min_binary bigint NOT NULL CONSTRAINT schema_compat_min_valid CHECK (min_binary >= 0)
);
-- Binaries before this one stop on a newer schema altogether, so this migration is breaking for them; for the next release on it is the first with a range.
INSERT INTO schema_compat (version, min_binary) VALUES (33, 33);
