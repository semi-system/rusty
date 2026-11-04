-- The ON DELETE RESTRICT foreign key messages.blob_hash -> blobs.hash is checked for every
-- blobs row that garbage collection deletes. SQLite probes the child table for each one, and
-- without an index that is a full scan of messages per deleted blob (review PG5 I1).
CREATE INDEX messages_blob_hash ON messages (blob_hash);
