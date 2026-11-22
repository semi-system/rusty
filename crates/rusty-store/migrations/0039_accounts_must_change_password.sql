-- An administrator can give an account a temporary password that must be changed before the account is used (docs/design-domain-settings.md): the flag is set with the password and cleared
-- when the user (or an administrator, without the flag) sets another. The services refuse to sign in an account that has it, with a message that says why.
ALTER TABLE accounts ADD COLUMN must_change_password boolean NOT NULL DEFAULT false;
INSERT INTO schema_compat (version, min_binary) VALUES (39, 38);
