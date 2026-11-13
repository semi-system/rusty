-- DNS validation persistence (plan U10, "Persistence for later re-validation"): the expected
-- state of a domain's DNS records and what the last check saw, so a record that is deleted or
-- changed at the registrar later is detected. The records themselves are NOT stored (always
-- regenerated from configuration and key); no secret is stored: only public selectors, hashes
-- and truncated evidence. Tenant isolation: every row carries its domain and goes with it
-- (ON DELETE CASCADE).
--
-- PostgreSQL twin: migrations_pg/0014_domain_dns.sql (the PG twin takes the SQLite number).

-- One row per domain: what the generated record set was built from, and the hash of that set at
-- the last "apply" (the admin said the records were published).
CREATE TABLE domain_dns_config (
  domain_id            INTEGER PRIMARY KEY REFERENCES domains(id) ON DELETE CASCADE,
  dkim_selector        TEXT CHECK (dkim_selector IS NULL OR length(dkim_selector) BETWEEN 1 AND 63),
  dkim_algorithm       TEXT CHECK (dkim_algorithm IS NULL OR dkim_algorithm IN ('rsa','ed25519')),
  dmarc_policy         TEXT NOT NULL DEFAULT 'none' CHECK (dmarc_policy IN ('none','quarantine','reject')),
  dmarc_rua            TEXT CHECK (dmarc_rua IS NULL OR length(dmarc_rua) BETWEEN 3 AND 254),
  -- Reference to the outbound identity (an address or a relay name), not a credential.
  outbound_ref         TEXT CHECK (outbound_ref IS NULL OR length(outbound_ref) BETWEEN 1 AND 255),
  -- SHA-256 (hex) of the ownership challenge token; the token itself is never stored.
  ownership_token_hash TEXT CHECK (ownership_token_hash IS NULL OR length(ownership_token_hash) = 64),
  expected_hash        TEXT CHECK (expected_hash IS NULL OR length(expected_hash) = 64),
  created_at           INTEGER NOT NULL,
  updated_at           INTEGER NOT NULL
) STRICT;

-- One row per domain and record key: expected value hash and the last observation.
CREATE TABLE domain_dns_state (
  domain_id            INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  record_name          TEXT NOT NULL CHECK (length(record_name) BETWEEN 1 AND 255),
  record_type          TEXT NOT NULL CHECK (record_type IN ('MX','A','AAAA','TXT','SRV','PTR','CNAME')),
  purpose              TEXT NOT NULL CHECK (length(purpose) BETWEEN 1 AND 128),
  expected_value_hash  TEXT NOT NULL CHECK (length(expected_value_hash) = 64),
  last_status          TEXT NOT NULL CHECK (last_status IN ('ok','missing','mismatch','warning','pending')),
  last_seen_value      TEXT CHECK (last_seen_value IS NULL OR length(CAST(last_seen_value AS BLOB)) <= 512),
  first_ok_at          INTEGER,
  last_ok_at           INTEGER,
  last_checked_at      INTEGER NOT NULL,
  status_since         INTEGER NOT NULL,
  consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
  PRIMARY KEY (domain_id, record_name, record_type, purpose)
) STRICT;
