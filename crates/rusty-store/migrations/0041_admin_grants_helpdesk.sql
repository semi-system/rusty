-- The helpdesk role (docs/design-helpdesk.md): a domain administrator that may only look at the accounts of its domain, set their passwords and lock or unlock them. The flag is on the grant, so
-- a revoke removes it with the rest. Grant the role only after every admin instance runs this version: an older instance does not know the flag and treats the grant as a full domain administrator.
ALTER TABLE admin_grants ADD COLUMN helpdesk boolean NOT NULL DEFAULT false CONSTRAINT admin_grants_helpdesk_domain CHECK (NOT helpdesk OR domain_id IS NOT NULL);
INSERT INTO schema_compat (version, min_binary) VALUES (41, 40);
