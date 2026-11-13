-- Twin of SQLite migration 0014_domain_dns.sql (see there for the model). Timestamps are bigint
-- unix seconds like every other table; `rs_now()` is the database clock.
CREATE TABLE domain_dns_config (
  domain_id            bigint PRIMARY KEY REFERENCES domains(id) ON DELETE CASCADE,
  dkim_selector        text COLLATE "C" CONSTRAINT domain_dns_config_selector_valid
                       CHECK (dkim_selector IS NULL OR length(dkim_selector) BETWEEN 1 AND 63),
  dkim_algorithm       text COLLATE "C" CONSTRAINT domain_dns_config_algorithm_valid
                       CHECK (dkim_algorithm IS NULL OR dkim_algorithm IN ('rsa','ed25519')),
  dmarc_policy         text COLLATE "C" NOT NULL DEFAULT 'none' CONSTRAINT domain_dns_config_policy_valid
                       CHECK (dmarc_policy IN ('none','quarantine','reject')),
  dmarc_rua            text COLLATE "C" CONSTRAINT domain_dns_config_rua_valid
                       CHECK (dmarc_rua IS NULL OR length(dmarc_rua) BETWEEN 3 AND 254),
  outbound_ref         text COLLATE "C" CONSTRAINT domain_dns_config_outbound_valid
                       CHECK (outbound_ref IS NULL OR length(outbound_ref) BETWEEN 1 AND 255),
  ownership_token_hash text COLLATE "C" CONSTRAINT domain_dns_config_token_valid
                       CHECK (ownership_token_hash IS NULL OR length(ownership_token_hash) = 64),
  expected_hash        text COLLATE "C" CONSTRAINT domain_dns_config_expected_valid
                       CHECK (expected_hash IS NULL OR length(expected_hash) = 64),
  created_at           bigint NOT NULL,
  updated_at           bigint NOT NULL
);

CREATE TABLE domain_dns_state (
  domain_id            bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  record_name          text COLLATE "C" NOT NULL CONSTRAINT domain_dns_state_name_valid
                       CHECK (length(record_name) BETWEEN 1 AND 255),
  record_type          text COLLATE "C" NOT NULL CONSTRAINT domain_dns_state_type_valid
                       CHECK (record_type IN ('MX','A','AAAA','TXT','SRV','PTR','CNAME')),
  purpose              text COLLATE "C" NOT NULL CONSTRAINT domain_dns_state_purpose_valid
                       CHECK (length(purpose) BETWEEN 1 AND 128),
  expected_value_hash  text COLLATE "C" NOT NULL CONSTRAINT domain_dns_state_hash_valid
                       CHECK (length(expected_value_hash) = 64),
  last_status          text COLLATE "C" NOT NULL CONSTRAINT domain_dns_state_status_valid
                       CHECK (last_status IN ('ok','missing','mismatch','warning','pending')),
  last_seen_value      text COLLATE "C" CONSTRAINT domain_dns_state_seen_valid
                       CHECK (last_seen_value IS NULL OR octet_length(last_seen_value) <= 512),
  first_ok_at          bigint,
  last_ok_at           bigint,
  last_checked_at      bigint NOT NULL,
  status_since         bigint NOT NULL,
  consecutive_failures bigint NOT NULL DEFAULT 0 CONSTRAINT domain_dns_state_failures_valid
                       CHECK (consecutive_failures >= 0),
  PRIMARY KEY (domain_id, record_name, record_type, purpose)
);
