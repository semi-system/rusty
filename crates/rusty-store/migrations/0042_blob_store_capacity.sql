-- The capacity of a tenant's blob store (docs/design-storage-capacity.md). The S3 protocol cannot say how much room a store has left, so the operator declares it (`capacity_bytes`) and the
-- storage check compares it with what the tenant holds and has been promised: the bytes in its mailboxes (`used_bytes`), the sum of the quotas of its accounts (`promised_bytes`) and how many
-- of its accounts have no quota at all (`unlimited_accounts`). `capacity_status` is the verdict of the last check: `unset` (no capacity declared), `ok`, `overcommitted`, `nearly_full` or `full`.
ALTER TABLE blob_stores ADD COLUMN capacity_bytes bigint CONSTRAINT blob_stores_capacity_valid CHECK (capacity_bytes IS NULL OR capacity_bytes > 0);
ALTER TABLE blob_stores ADD COLUMN used_bytes bigint;
ALTER TABLE blob_stores ADD COLUMN promised_bytes bigint;
ALTER TABLE blob_stores ADD COLUMN unlimited_accounts bigint;
ALTER TABLE blob_stores ADD COLUMN capacity_status text COLLATE "C" NOT NULL DEFAULT 'unset' CONSTRAINT blob_stores_capacity_status_valid CHECK (capacity_status IN ('unset', 'ok', 'overcommitted', 'nearly_full', 'full'));
-- Additive: a binary of the previous schema never reads the columns.
INSERT INTO schema_compat (version, min_binary) VALUES (42, 41);
