-- What a change set (docs/design-audit.md): the fields of the request body as a small JSON object of text values, with every value whose name suggests a secret replaced before it is written. Null for
-- a change without a body (a delete) and for the rows written before this column.
ALTER TABLE audit_log ADD COLUMN detail text COLLATE "C" CONSTRAINT audit_log_detail_valid CHECK (detail IS NULL OR length(detail) <= 4096);
-- Additive: a binary of the previous schema neither reads nor writes the column.
INSERT INTO schema_compat (version, min_binary) VALUES (47, 46);
