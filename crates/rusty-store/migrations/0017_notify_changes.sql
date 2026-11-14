-- Cross-process notifications. With one process per service (smtp-in, imap, submission, outbound) a delivery
-- or a queued job can no longer be announced through memory, so the database announces it. NOTIFY is delivered
-- at COMMIT: a rolled-back change is never announced, and no application code has to remember to call it.
--
--   channel rusty_mailbox  payload '<mailbox id>:<newest change seq>'   (IMAP IDLE)
--   channel rusty_jobs     payload ''                                    (the outbound worker)
--
-- The triggers are statement-level with a transition table, so one statement that adds 200 change-log rows
-- sends ONE notification per mailbox (the newest seq), not 200. A notification is a hint, not a record: a
-- listener that was away resynchronises from the change log or the job queue, and both consumers also keep a
-- timer, so nothing depends on delivery. The functions run as the caller (no SECURITY DEFINER: pg_notify needs
-- no privilege) and pin search_path with pg_temp last, as the definer functions of 0016 do.

CREATE FUNCTION rs_notify_mailboxes() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
  r record;
BEGIN
  FOR r IN SELECT mailbox_id, max(seq) AS newest FROM new_rows GROUP BY mailbox_id LOOP
    PERFORM pg_notify('rusty_mailbox', r.mailbox_id::text || ':' || r.newest::text);
  END LOOP;
  RETURN NULL;
END
$$;

CREATE TRIGGER change_log_notify
  AFTER INSERT ON change_log
  REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION rs_notify_mailboxes();

CREATE FUNCTION rs_notify_jobs() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
  PERFORM pg_notify('rusty_jobs', '');
  RETURN NULL;
END
$$;

CREATE TRIGGER jobs_notify
  AFTER INSERT ON jobs
  FOR EACH STATEMENT EXECUTE FUNCTION rs_notify_jobs();
