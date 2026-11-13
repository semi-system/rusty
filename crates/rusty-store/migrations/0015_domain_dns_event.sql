-- Twin of SQLite migration 0015_domain_dns_event.sql (see there).
CREATE TABLE domain_dns_event (
  id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  domain_id    bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  record_name  text COLLATE "C" NOT NULL CONSTRAINT domain_dns_event_name_valid
               CHECK (length(record_name) BETWEEN 1 AND 255),
  record_type  text COLLATE "C" NOT NULL CONSTRAINT domain_dns_event_type_valid
               CHECK (record_type IN ('MX','A','AAAA','TXT','SRV','PTR','CNAME')),
  purpose      text COLLATE "C" NOT NULL CONSTRAINT domain_dns_event_purpose_valid
               CHECK (length(purpose) BETWEEN 1 AND 128),
  from_status  text COLLATE "C" CONSTRAINT domain_dns_event_from_valid
               CHECK (from_status IS NULL OR from_status IN ('ok','missing','mismatch','warning','pending')),
  to_status    text COLLATE "C" NOT NULL CONSTRAINT domain_dns_event_to_valid
               CHECK (to_status IN ('ok','missing','mismatch','warning','pending')),
  at           bigint NOT NULL
);
CREATE INDEX domain_dns_event_by_domain ON domain_dns_event (domain_id, id);
