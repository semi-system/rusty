-- Fair claiming of jobs between tenants (docs/design-fair-claim.md): a worker that asks for a job gets the oldest job of the tenant that was served least recently, not the oldest job of the
-- whole queue, so a tenant with a large backlog does not make the others wait behind it. A tenant is a domain; jobs with no domain (platform jobs) are one more tenant, scope 0.
--
-- The index serves the "tenants that have queued jobs" skip scan and the oldest queued job of one tenant (the expression is the scope of a job).
CREATE INDEX jobs_queued_scope ON jobs ((COALESCE(domain_id, 0)), run_after, id) WHERE state = 'queued';

-- The turn order: one row per (tenant, kind of job), the number of its last claim from one global sequence (no clock ties). A tenant with no row has never been served and goes first.
CREATE SEQUENCE job_turn_seq;
CREATE TABLE job_turns (
  scope      bigint NOT NULL CONSTRAINT job_turns_scope_valid CHECK (scope >= 0),
  kind       text COLLATE "C" NOT NULL CONSTRAINT job_turns_kind_valid CHECK (length(kind) BETWEEN 1 AND 64),
  last_claim bigint NOT NULL,
  PRIMARY KEY (scope, kind)
);
