-- The delivery log also records what arrived (docs/design-queue.md): `direction` is `out` for the outbound worker's attempts (every existing row) and `in` for mail accepted on port 25 and
-- delivered to a mailbox, or refused after the DATA command (by a filter or by the rules of a list). Additive: the old binary writes rows without it and they are `out`.
ALTER TABLE mail_log ADD COLUMN direction text COLLATE "C" NOT NULL DEFAULT 'out' CONSTRAINT mail_log_direction_valid CHECK (direction IN ('out', 'in'));
INSERT INTO schema_compat (version, min_binary) VALUES (38, 37);
