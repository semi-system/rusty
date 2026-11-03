-- Per-domain pending-job cap (jobs.rs counts queued + running jobs of one domain_id inside the
-- enqueue transaction) and the ON DELETE RESTRICT check on domains(id) both look jobs up by
-- domain_id; without this index each is a full table scan. domain_id IS NULL (jobs without a
-- domain) is served by the same index.
CREATE INDEX jobs_domain_state ON jobs (domain_id, state);
