-- Collection leases: one row per blob store while a garbage collector works on it. The in-process mutex of an S3 store and the lock file of a directory store cannot keep two
-- collectors of different processes (or hosts) off one bucket; this row can. A lease expires on the database clock, so a collector that died does not block the store forever.
CREATE TABLE blob_store_leases (
  store_id   bigint PRIMARY KEY REFERENCES blob_stores(id) ON DELETE CASCADE,
  holder     text COLLATE "C" NOT NULL CONSTRAINT blob_store_leases_holder_valid CHECK (length(holder) BETWEEN 1 AND 128),
  expires_at bigint NOT NULL
);
