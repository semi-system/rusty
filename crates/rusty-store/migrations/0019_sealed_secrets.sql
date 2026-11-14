-- Secrets that the web creates or receives (a DKIM private key, a tenant relay password), sealed to the outbound
-- service's public key (HPKE, crate `rusty-seal`) before they are stored. The database never holds a plaintext and cannot open
-- what it holds: only the outbound service has the private half of the key.
--
-- One row per (owner kind, owner id, purpose). A write replaces the whole row. `owner_kind` is generic on purpose (`domain` today)
-- and carries no foreign key; the trigger below removes the secrets of a deleted domain.
--
-- Privileges (deploy/db/roles.sql and grants_pg.sql): the admin service gets INSERT, UPDATE and DELETE and no SELECT on the
-- ciphertext; the outbound service gets SELECT. Until the per-service roles exist the store layer has the same split (a writer
-- facade that cannot read a ciphertext, a reader facade that cannot write).
CREATE TABLE sealed_secrets (
  owner_kind text COLLATE "C" NOT NULL CONSTRAINT sealed_secrets_kind_valid
             CHECK (owner_kind ~ '^[a-z0-9_-]{1,32}$'),
  owner_id   bigint NOT NULL,
  purpose    text COLLATE "C" NOT NULL CONSTRAINT sealed_secrets_purpose_valid
             CHECK (purpose ~ '^[a-z0-9_-]{1,32}$'),
  key_id     text COLLATE "C" NOT NULL CONSTRAINT sealed_secrets_key_id_valid
             CHECK (key_id ~ '^[0-9a-f]{16}$'),
  sealed     bytea NOT NULL CONSTRAINT sealed_secrets_size_valid
             CHECK (octet_length(sealed) BETWEEN 16 AND 66000),
  created_at bigint NOT NULL,
  updated_at bigint NOT NULL,
  PRIMARY KEY (owner_kind, owner_id, purpose)
);

CREATE FUNCTION rs_sealed_domain_cleanup() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
  DELETE FROM sealed_secrets WHERE owner_kind = 'domain' AND owner_id = OLD.id;
  RETURN OLD;
END
$$;

CREATE TRIGGER domains_sealed_cleanup
  AFTER DELETE ON domains
  FOR EACH ROW EXECUTE FUNCTION rs_sealed_domain_cleanup();
