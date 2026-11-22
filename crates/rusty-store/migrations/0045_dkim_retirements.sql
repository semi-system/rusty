-- The automatic end of an old DKIM key after a rotation (docs/design-dkim-rotation.md): when a new key is made for a domain that already had keys, each older key is scheduled here to be retired
-- a few days later (the time receivers need to stop using the old record). The admin service retires the due ones (the public row and the sealed private half) and clears the entry; a key that is
-- retired by hand loses its entry.
CREATE TABLE dkim_retirements (
  domain_id bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  selector  text COLLATE "C" NOT NULL CONSTRAINT dkim_retirements_selector_valid CHECK (length(selector) BETWEEN 1 AND 63),
  retire_at bigint NOT NULL CONSTRAINT dkim_retirements_at_valid CHECK (retire_at >= 0),
  PRIMARY KEY (domain_id, selector)
);
-- The scan for entries that are due.
CREATE INDEX dkim_retirements_at ON dkim_retirements (retire_at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (45, 44);
