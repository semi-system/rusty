-- Bounded history of DNS record status changes (plan U10): `ok -> missing` at time T. Status
-- transitions only, capped per domain by the repository (DNS_EVENTS_PER_DOMAIN). Same tenant
-- rules as 0014: the rows go with their domain.
--
-- PostgreSQL twin: migrations_pg/0015_domain_dns_event.sql.
CREATE TABLE domain_dns_event (
  id           INTEGER PRIMARY KEY,
  domain_id    INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  record_name  TEXT NOT NULL CHECK (length(record_name) BETWEEN 1 AND 255),
  record_type  TEXT NOT NULL CHECK (record_type IN ('MX','A','AAAA','TXT','SRV','PTR','CNAME')),
  purpose      TEXT NOT NULL CHECK (length(purpose) BETWEEN 1 AND 128),
  from_status  TEXT CHECK (from_status IS NULL OR from_status IN ('ok','missing','mismatch','warning','pending')),
  to_status    TEXT NOT NULL CHECK (to_status IN ('ok','missing','mismatch','warning','pending')),
  at           INTEGER NOT NULL
) STRICT;
CREATE INDEX domain_dns_event_by_domain ON domain_dns_event (domain_id, id);
