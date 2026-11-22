-- When mail last ARRIVED in a mailbox by delivery (SMTP, or a local recipient of a submission), as the admin web shows it ("last delivery"). Written by the statement that takes the message ids
-- of a delivery, so it costs no statement of its own; an IMAP APPEND or a copy does not touch it. `NULL`: nothing has been delivered yet.
ALTER TABLE mailboxes ADD COLUMN last_delivery_at bigint CONSTRAINT mailboxes_last_delivery_valid CHECK (last_delivery_at IS NULL OR last_delivery_at >= 0);
INSERT INTO schema_compat (version, min_binary) VALUES (37, 36);
