-- Twin of SQLite migration 0012_folder_subscribed.sql (see there). The column is bigint 0/1 so
-- both backends decode it the same way.
ALTER TABLE folders ADD COLUMN subscribed bigint NOT NULL DEFAULT 1
  CONSTRAINT folders_subscribed_valid CHECK (subscribed IN (0, 1));
