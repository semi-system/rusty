-- When an account was closed (docs/design-delete.md): set when its status becomes `disabled`, cleared when it is enabled or locked again. A closed account whose `closed_at` is older than the
-- retention (30 days by default; `0` is an administrator's "delete now") is removed by the garbage collector together with its mailbox. `NULL`: not closed.
ALTER TABLE accounts ADD COLUMN closed_at bigint CONSTRAINT accounts_closed_at_valid CHECK (closed_at IS NULL OR closed_at >= 0);
UPDATE accounts SET closed_at = rs_now() WHERE status = 'disabled';
-- Additive: a binary of the previous schema keeps working (it never reads the column, and its status update leaves it as it is).
INSERT INTO schema_compat (version, min_binary) VALUES (34, 33);
