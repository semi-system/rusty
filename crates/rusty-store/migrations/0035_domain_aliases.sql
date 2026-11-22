-- Domain aliases (docs/design-routing.md): `alias.test` receives the mail of `real.test`: `bob@alias.test` is delivered as `bob@real.test`. The alias name is NOT a hosted domain (a hosted domain has
-- its own accounts); it only points at one. One name belongs to one domain, and a domain's aliases go with it.
CREATE TABLE domain_aliases (
  name      text COLLATE "C" PRIMARY KEY CONSTRAINT domain_aliases_name_valid CHECK (length(name) BETWEEN 1 AND 253 AND name = lower(name)),
  domain_id bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE
);
CREATE INDEX domain_aliases_domain ON domain_aliases (domain_id);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (35, 34);
