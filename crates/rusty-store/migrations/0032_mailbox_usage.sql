-- Usage of a mailbox in bytes (docs/design-plans-quota.md): the sum of `messages.size` of the messages the mailbox still has (`deleted_at IS NULL`: an expunged message is gone for its owner and
-- no longer counts, though its row waits for the purge), kept by the database in the transaction that adds, removes or expunges messages. The quota check at delivery then reads one number
-- from the mailbox row the transaction already holds locked: no sum over the messages, and no drift between a counter and its rows.
ALTER TABLE mailboxes ADD COLUMN used_bytes bigint NOT NULL DEFAULT 0 CONSTRAINT mailboxes_used_bytes_valid CHECK (used_bytes >= 0);
UPDATE mailboxes m SET used_bytes = COALESCE((SELECT sum(size) FROM messages WHERE mailbox_id = m.id AND deleted_at IS NULL), 0);

-- One statement per insert, delete or update statement (a batch of a thousand messages is one update per mailbox, not a thousand). SECURITY DEFINER: the services that change messages hold
-- no write on `mailboxes` of their own; the function is theirs only through the trigger.
CREATE FUNCTION rs_messages_usage_ins() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
  UPDATE mailboxes m SET used_bytes = m.used_bytes + s.total
    FROM (SELECT mailbox_id, sum(size) AS total FROM new_rows WHERE deleted_at IS NULL GROUP BY mailbox_id) s WHERE m.id = s.mailbox_id;
  RETURN NULL;
END;
$$;
CREATE FUNCTION rs_messages_usage_del() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
  UPDATE mailboxes m SET used_bytes = greatest(m.used_bytes - s.total, 0)
    FROM (SELECT mailbox_id, sum(size) AS total FROM old_rows WHERE deleted_at IS NULL GROUP BY mailbox_id) s WHERE m.id = s.mailbox_id;
  RETURN NULL;
END;
$$;
-- An update that expunges (sets `deleted_at`) or restores a message moves its bytes out of or into the usage.
CREATE FUNCTION rs_messages_usage_upd() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
  UPDATE mailboxes m SET used_bytes = greatest(m.used_bytes + s.delta, 0)
    FROM (SELECT n.mailbox_id, sum(CASE WHEN n.deleted_at IS NULL THEN n.size ELSE 0 END - CASE WHEN o.deleted_at IS NULL THEN o.size ELSE 0 END) AS delta
            FROM new_rows n JOIN old_rows o ON o.mailbox_id = n.mailbox_id AND o.id = n.id GROUP BY n.mailbox_id) s
   WHERE m.id = s.mailbox_id AND s.delta <> 0;
  RETURN NULL;
END;
$$;
REVOKE ALL ON FUNCTION rs_messages_usage_ins() FROM PUBLIC;
REVOKE ALL ON FUNCTION rs_messages_usage_del() FROM PUBLIC;
REVOKE ALL ON FUNCTION rs_messages_usage_upd() FROM PUBLIC;
CREATE TRIGGER messages_usage_ins AFTER INSERT ON messages REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rs_messages_usage_ins();
CREATE TRIGGER messages_usage_del AFTER DELETE ON messages REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rs_messages_usage_del();
CREATE TRIGGER messages_usage_upd AFTER UPDATE ON messages REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rs_messages_usage_upd();
