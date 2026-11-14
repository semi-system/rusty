-- The public half of the DKIM keys of a domain, so that the admin web (which never reads a private key) can show the
-- record to publish and the DNS check can compare it with what is published. The private keys stay in files of the
-- outbound service (`dkim_dir`); `rustyctl dkim generate` and `dkim import` write the public half here.
--
-- One row per published selector (an ed25519 key and an RSA key of one domain are two rows). A row is written once
-- and never changes: a new key is a new selector. The trigger below makes that true for any writer, not only for
-- the store layer.
CREATE TABLE domain_dkim_keys (
  domain_id      bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  selector       text COLLATE "C" NOT NULL CONSTRAINT domain_dkim_keys_selector_valid
                 CHECK (length(selector) BETWEEN 1 AND 63 AND selector ~ '^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$'),
  algorithm      text COLLATE "C" NOT NULL CONSTRAINT domain_dkim_keys_algorithm_valid
                 CHECK (algorithm IN ('rsa','ed25519')),
  public_key_b64 text COLLATE "C" NOT NULL CONSTRAINT domain_dkim_keys_key_valid
                 CHECK (length(public_key_b64) BETWEEN 16 AND 1024 AND public_key_b64 ~ '^[A-Za-z0-9+/]+={0,2}$'),
  created_at     bigint NOT NULL,
  PRIMARY KEY (domain_id, selector)
);

CREATE FUNCTION rs_dkim_keys_immutable() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
  RAISE EXCEPTION 'a published DKIM key never changes: use a new selector' USING ERRCODE = '23514';
END
$$;

CREATE TRIGGER domain_dkim_keys_immutable
  BEFORE UPDATE ON domain_dkim_keys
  FOR EACH ROW EXECUTE FUNCTION rs_dkim_keys_immutable();
