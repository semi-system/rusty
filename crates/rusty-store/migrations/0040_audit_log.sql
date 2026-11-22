-- The audit log of the admin web (docs/design-audit.md): one row per change an administrator made, with who, when, from which address and what. Written by the admin service, read by its audit
-- page (a domain administrator sees the rows of its own domain), removed by the garbage collector after 365 days. No request body is kept (it may hold a password).
CREATE TABLE audit_log (
  id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at        bigint NOT NULL CONSTRAINT audit_log_at_valid CHECK (at >= 0),
  -- The account that acted; no foreign key: the history outlives the account. The address is kept as it was.
  actor_id  bigint,
  actor     text COLLATE "C" NOT NULL CONSTRAINT audit_log_actor_valid CHECK (length(actor) <= 320),
  -- The domain the change concerned (also without a foreign key); `NULL` for a platform-wide change.
  domain_id bigint,
  ip        text COLLATE "C" NOT NULL CONSTRAINT audit_log_ip_valid CHECK (length(ip) <= 64),
  method    text COLLATE "C" NOT NULL CONSTRAINT audit_log_method_valid CHECK (method IN ('POST', 'PUT', 'PATCH', 'DELETE')),
  -- The route as registered (`/api/v1/domains/{d}/accounts`) and the path as sent, without the query string.
  route     text COLLATE "C" NOT NULL CONSTRAINT audit_log_route_valid CHECK (length(route) <= 200),
  path      text COLLATE "C" NOT NULL CONSTRAINT audit_log_path_valid CHECK (length(path) <= 512),
  status    integer NOT NULL
);
-- A domain's rows, newest first.
CREATE INDEX audit_log_domain ON audit_log (domain_id, id DESC);
-- The cleanup of rows past their retention.
CREATE INDEX audit_log_at ON audit_log (at);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (40, 39);
