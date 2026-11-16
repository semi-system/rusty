-- Whether an account has a password, as a column of its own: queries that only need to know that (listing accounts, finding one for delivery) read `password_set` and never the hash,
-- so the services that do not check passwords can be refused the `password_hash` column altogether (grants_pg.sql).
ALTER TABLE accounts ADD COLUMN password_set boolean GENERATED ALWAYS AS (password_hash IS NOT NULL) STORED;
