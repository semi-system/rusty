-- Garbage collection support (spec §4/§5).
--
-- blobs.zero_since: NULL while the blob is referenced (refcount > 0); otherwise the unix time
-- (seconds) at which refcount last became 0. GC deletes a row only once zero_since is older
-- than its grace window. Maintained only by the triggers below; application code never
-- writes it.
ALTER TABLE blobs ADD COLUMN zero_since INTEGER CHECK (zero_since IS NULL OR zero_since >= 0);

-- One-time backfill: unreferenced blobs that predate this migration start their grace now.
UPDATE blobs SET zero_since = unixepoch() WHERE refcount = 0;

CREATE TRIGGER blobs_zero_since_ins AFTER INSERT ON blobs
WHEN NEW.refcount = 0
BEGIN
  UPDATE blobs SET zero_since = unixepoch() WHERE hash = NEW.hash;
END;

-- Fires for the refcount updates done by the messages_refcount_* triggers.
CREATE TRIGGER blobs_zero_since_released AFTER UPDATE OF refcount ON blobs
WHEN NEW.refcount = 0 AND OLD.refcount <> 0
BEGIN
  UPDATE blobs SET zero_since = unixepoch() WHERE hash = NEW.hash;
END;

CREATE TRIGGER blobs_zero_since_referenced AFTER UPDATE OF refcount ON blobs
WHEN NEW.refcount > 0
BEGIN
  UPDATE blobs SET zero_since = NULL WHERE hash = NEW.hash;
END;

-- purge_deleted scans soft-deleted messages by age.
CREATE INDEX messages_deleted_at ON messages (deleted_at) WHERE deleted_at IS NOT NULL;
