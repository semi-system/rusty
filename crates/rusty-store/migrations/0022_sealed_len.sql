-- The size of a sealed secret as a column of its own, so that a role without SELECT on the ciphertext column (the admin service: deploy/db/roles.sql,
-- grants_pg.sql) can still learn that a secret exists and how large it is without ever reading it. `octet_length(sealed)` in a query would need
-- SELECT on `sealed` itself.
ALTER TABLE sealed_secrets ADD COLUMN sealed_len bigint GENERATED ALWAYS AS (octet_length(sealed)) STORED;
