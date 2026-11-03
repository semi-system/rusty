-- Idempotent delivery (distributed-model spec section 4).
--
-- One row per accepted delivery, written in the same transaction as the message. A retried
-- delivery with the same delivery_id finds the row and returns the original message instead
-- of creating a second one. delivery_id is SHA-256 over (content hash, recipient, session);
-- blob_hash is kept so a reused id with different content is detected. No FK to blobs: the
-- row must not pin a blob (messages already do). Rows cascade away with their message
-- (purge_deleted) and are also removed by gc::purge_deliveries after the retention window.
CREATE TABLE deliveries (
  mailbox_id  INTEGER NOT NULL,
  delivery_id BLOB NOT NULL CHECK (length(delivery_id) = 32),
  message_id  INTEGER NOT NULL,
  blob_hash   TEXT NOT NULL CHECK (length(blob_hash) = 64),
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (mailbox_id, delivery_id),
  FOREIGN KEY (mailbox_id, message_id) REFERENCES messages(mailbox_id, id) ON DELETE CASCADE
) STRICT;
CREATE INDEX deliveries_created ON deliveries (created_at);
CREATE INDEX deliveries_message ON deliveries (mailbox_id, message_id);
