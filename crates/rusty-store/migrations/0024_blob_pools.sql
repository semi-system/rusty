-- Blob pools: a tenant's store may be spread over several backends (docs/design-s3-per-tenant.md, section 12). A store of kind 'pool' has one to eight backends and a policy in
-- blob_stores.config (copies, sync_copies, placement, read). A store of kind 'fs', 's3' or 'platform' has no backend rows: it is its own single backend, exactly as before.
--
-- blob_locations says which backend holds which object. It is the authority for reads; the placement function is only how a new location is chosen. It has no foreign key to blobs:
-- a location is bookkeeping about an object in a bucket, and it has to outlive the blob row for as long as a backend that was down when the object was deleted still holds the bytes
-- ('pending_delete').
ALTER TABLE blob_stores DROP CONSTRAINT blob_stores_kind_valid;
ALTER TABLE blob_stores ADD CONSTRAINT blob_stores_kind_valid CHECK (kind IN ('platform', 'fs', 's3', 'pool'));

CREATE TABLE blob_backends (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  store_id     bigint NOT NULL REFERENCES blob_stores(id) ON DELETE RESTRICT,
  name         text COLLATE "C" NOT NULL CONSTRAINT blob_backends_name_valid CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,31}$'),
  -- What the backend is: a bucket or (for development and tests) a directory. Credentials are sealed secrets (owner kind 'blobbackend'), never here.
  kind         text COLLATE "C" NOT NULL CONSTRAINT blob_backends_kind_valid CHECK (kind IN ('fs', 's3')),
  config       jsonb NOT NULL DEFAULT '{}'::jsonb,
  weight       integer NOT NULL DEFAULT 100 CONSTRAINT blob_backends_weight_valid CHECK (weight BETWEEN 1 AND 1000),
  -- active: reads and writes. readonly: reads, no new objects. draining: the repair job moves its objects away. disabled: not used at all.
  state        text COLLATE "C" NOT NULL DEFAULT 'active' CONSTRAINT blob_backends_state_valid CHECK (state IN ('active', 'readonly', 'draining', 'disabled')),
  check_status text COLLATE "C" NOT NULL DEFAULT 'unchecked' CONSTRAINT blob_backends_check_valid CHECK (length(check_status) BETWEEN 1 AND 64),
  checked_at   bigint,
  created_at   bigint NOT NULL,
  CONSTRAINT blob_backends_name_unique UNIQUE (store_id, name)
);

CREATE TABLE blob_locations (
  store_id   bigint NOT NULL REFERENCES blob_stores(id) ON DELETE RESTRICT,
  hash       bytea NOT NULL CONSTRAINT blob_locations_hash_len CHECK (length(hash) = 32),
  backend_id bigint NOT NULL REFERENCES blob_backends(id) ON DELETE RESTRICT,
  -- ok: the object is there. pending_delete: the blob is gone, the backend could not be reached to delete it; the delete is retried.
  state      text COLLATE "C" NOT NULL DEFAULT 'ok' CONSTRAINT blob_locations_state_valid CHECK (state IN ('ok', 'pending_delete')),
  PRIMARY KEY (store_id, hash, backend_id)
);
-- What a backend holds (to drain it, to refuse removing it while it holds something) and what is waiting to be deleted.
CREATE INDEX blob_locations_backend ON blob_locations (backend_id, state);
