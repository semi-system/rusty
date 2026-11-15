-- Blob stores: where the body of a message lives. A tenant (a domain) may have a store of its own (one S3 bucket, bound to that domain only); every other domain, and system mail,
-- uses the platform store, row 1, whose settings come from the configuration file and never from the database (docs/design-s3-per-tenant.md).
--
-- This migration only makes room: every existing row belongs to the platform store (the column defaults), so nothing moves and the statements that do not name a store keep their meaning
-- while only store 1 has rows. The blob key becomes (store, hash): the same content in two stores is two objects and two rows.
CREATE TABLE blob_stores (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  -- The one domain that owns the store; NULL only for the platform store.
  domain_id    bigint UNIQUE REFERENCES domains(id) ON DELETE RESTRICT,
  kind         text COLLATE "C" NOT NULL CONSTRAINT blob_stores_kind_valid CHECK (kind IN ('platform', 'fs', 's3')),
  -- Non-secret settings only (endpoint, region, bucket, prefix, path style). Credentials are sealed secrets, never here.
  config       jsonb NOT NULL DEFAULT '{}'::jsonb,
  check_status text COLLATE "C" NOT NULL DEFAULT 'unchecked' CONSTRAINT blob_stores_check_valid CHECK (length(check_status) BETWEEN 1 AND 64),
  checked_at   bigint,
  created_at   bigint NOT NULL,
  CONSTRAINT blob_stores_platform_shape CHECK ((kind = 'platform') = (domain_id IS NULL))
);
INSERT INTO blob_stores (id, domain_id, kind, created_at) OVERRIDING SYSTEM VALUE VALUES (1, NULL, 'platform', rs_now());
SELECT setval(pg_get_serial_sequence('blob_stores', 'id'), 1);

ALTER TABLE domains ADD COLUMN blob_store_id bigint NOT NULL DEFAULT 1 REFERENCES blob_stores(id) ON DELETE RESTRICT;

ALTER TABLE blobs ADD COLUMN store_id bigint NOT NULL DEFAULT 1 REFERENCES blob_stores(id) ON DELETE RESTRICT;
ALTER TABLE messages DROP CONSTRAINT messages_blob_hash_fkey;
ALTER TABLE blobs DROP CONSTRAINT blobs_pkey;
ALTER TABLE blobs ADD PRIMARY KEY (store_id, hash);

ALTER TABLE messages ADD COLUMN blob_store_id bigint NOT NULL DEFAULT 1;
ALTER TABLE messages ADD CONSTRAINT messages_blob_fkey FOREIGN KEY (blob_store_id, blob_hash) REFERENCES blobs(store_id, hash) ON DELETE RESTRICT;
-- The ON DELETE RESTRICT foreign key probes messages for every blobs row GC deletes (see 0001): the index follows the key.
DROP INDEX messages_blob_hash;
CREATE INDEX messages_blob ON messages (blob_store_id, blob_hash);

CREATE OR REPLACE FUNCTION rs_messages_refcount() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    UPDATE blobs SET refcount = refcount + 1 WHERE store_id = NEW.blob_store_id AND hash = NEW.blob_hash;
    RETURN NEW;
  ELSIF TG_OP = 'DELETE' THEN
    UPDATE blobs SET refcount = refcount - 1 WHERE store_id = OLD.blob_store_id AND hash = OLD.blob_hash;
    RETURN OLD;
  ELSIF OLD.blob_hash IS DISTINCT FROM NEW.blob_hash OR OLD.blob_store_id IS DISTINCT FROM NEW.blob_store_id THEN
    UPDATE blobs SET refcount = refcount - 1 WHERE store_id = OLD.blob_store_id AND hash = OLD.blob_hash;
    UPDATE blobs SET refcount = refcount + 1 WHERE store_id = NEW.blob_store_id AND hash = NEW.blob_hash;
  END IF;
  RETURN NEW;
END;
$$;
DROP TRIGGER messages_refcount_upd ON messages;
CREATE TRIGGER messages_refcount_upd AFTER UPDATE OF blob_hash, blob_store_id ON messages
  FOR EACH ROW EXECUTE FUNCTION rs_messages_refcount();
