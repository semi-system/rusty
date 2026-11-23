-- Hosted DNS zones (docs/design-authoritative-dns.md, section 7 step 3): a domain is answered by the platform's name servers (`hosted`) or by whoever the domain has today (`external`, the default: nothing changes
-- for a domain until its administrator chooses). A hosted domain has the records the platform generates for mail (MX, SPF, DKIM, DMARC, SRV, autoconfig) and, besides, the records its tenant adds here.
ALTER TABLE domains ADD COLUMN dns_mode text COLLATE "C" NOT NULL DEFAULT 'external' CONSTRAINT domains_dns_mode_valid CHECK (dns_mode IN ('external', 'hosted'));

CREATE TABLE domain_dns_extra (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  domain_id  bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  -- A name inside the domain, relative to it (`www`, `_acme-challenge.blog`), or `@` for the domain itself. No wildcard.
  name       text COLLATE "C" NOT NULL CONSTRAINT domain_dns_extra_name_valid CHECK (length(name) BETWEEN 1 AND 200 AND name ~ '^(@|[a-z0-9_]([a-z0-9_.-]*[a-z0-9_])?)$'),
  rtype      text COLLATE "C" NOT NULL CONSTRAINT domain_dns_extra_rtype_valid CHECK (rtype IN ('A', 'AAAA', 'CNAME', 'TXT', 'MX', 'SRV')),
  value      text COLLATE "C" NOT NULL CONSTRAINT domain_dns_extra_value_valid CHECK (length(value) BETWEEN 1 AND 2000 AND value !~ '[[:cntrl:]]'),
  ttl        integer NOT NULL DEFAULT 3600 CONSTRAINT domain_dns_extra_ttl_valid CHECK (ttl BETWEEN 60 AND 86400),
  created_at bigint NOT NULL
);
CREATE INDEX domain_dns_extra_domain ON domain_dns_extra (domain_id, id);
-- A tenant has at most 200 extra records (enforced in the repository; the index serves the count).
-- Additive: a binary of the previous schema never reads the column or the table.
INSERT INTO schema_compat (version, min_binary) VALUES (55, 54);
