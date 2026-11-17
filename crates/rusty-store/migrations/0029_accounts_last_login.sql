-- When an account last signed in (docs/design-admin-parity.md, item 2). Written by the services that check a password (imap, submission) and by the admin web, through the one statement of
-- `DirectoryRepo::record_login`: it changes the row only when the stored time is more than an hour old, so a client that signs in every few seconds writes once an hour (and the row trigger of
-- 0028 announces it once an hour, not at every connection). `NULL`: never signed in.
ALTER TABLE accounts ADD COLUMN last_login_at bigint CONSTRAINT accounts_last_login_valid CHECK (last_login_at IS NULL OR last_login_at >= 0);
