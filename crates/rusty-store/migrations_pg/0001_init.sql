-- Rusty PostgreSQL schema, consolidated from SQLite migrations 0001..0008.
--
-- Translation table:
--   SQLite INTEGER PRIMARY KEY       -> bigint GENERATED ALWAYS AS IDENTITY
--   STRICT                           -> native PostgreSQL column types
--   JSON text + json_valid           -> jsonb
--   64-char hex / 32-byte BLOB       -> bytea CHECK (octet_length(value) = 32)
--   GLOB allow-list                  -> anchored COLLATE "C" regular expression
--   unixepoch()                      -> rs_now() using statement_timestamp()
--   RAISE(ABORT, conflict/invalid)   -> SQLSTATE RS001 / RS002
--   rowid member order               -> list_members.seq, per-list, assigned by a trigger
--   partial/expression indexes       -> equivalent PostgreSQL indexes
--   BEGIN IMMEDIATE                  -> BEGIN + mailbox row FOR UPDATE (repository layer)
--
-- ON CONFLICT: on accounts, aliases and lists a duplicate address raises RS001 from the BEFORE
-- INSERT trigger before the arbiter index is consulted, so `ON CONFLICT DO NOTHING` does NOT
-- suppress it; repositories must map RS001
-- (Conflict). Plain UNIQUE violations on other tables raise 23505 and ON CONFLICT works there.

CREATE FUNCTION rs_now() RETURNS bigint
LANGUAGE sql STABLE PARALLEL SAFE
RETURN extract(epoch FROM statement_timestamp())::bigint;

CREATE TABLE domains (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name        text COLLATE "C" NOT NULL UNIQUE
              CONSTRAINT domains_name_valid CHECK (length(name) > 0 AND name = lower(name)),
  config      jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at  bigint NOT NULL
);

CREATE TABLE plans (
  id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name    text COLLATE "C" NOT NULL UNIQUE CONSTRAINT plans_name_valid CHECK (length(name) > 0),
  config  jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE accounts (
  id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  local_part    text COLLATE "C" NOT NULL
                CONSTRAINT accounts_local_part_valid
                CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id     bigint NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  plan_id       bigint REFERENCES plans(id) ON DELETE RESTRICT,
  password_hash text,
  status        text NOT NULL CONSTRAINT accounts_status_valid
                CHECK (status IN ('active','locked','disabled')),
  config        jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    bigint NOT NULL,
  UNIQUE (local_part, domain_id),
  CONSTRAINT accounts_id_domain UNIQUE (id, domain_id)
);

CREATE TABLE aliases (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  local_part  text COLLATE "C" NOT NULL
              CONSTRAINT aliases_local_part_valid
              CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   bigint NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  account_id  bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  UNIQUE (local_part, domain_id)
);

CREATE TABLE lists (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  local_part  text COLLATE "C" NOT NULL
              CONSTRAINT lists_local_part_valid
              CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   bigint NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  UNIQUE (local_part, domain_id)
);

CREATE TABLE list_members (
  list_id     bigint NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  account_id  bigint REFERENCES accounts(id) ON DELETE CASCADE,
  external    text COLLATE "C",
  -- Same semantics as SQLite 0008: per-list, dense, assigned when omitted (see the
  -- rs_list_members_seq trigger below). Callers on PostgreSQL must OMIT seq; an explicit value
  -- computed client side as MAX+1 would race and surface as a spurious UNIQUE violation.
  seq         bigint NOT NULL,
  CONSTRAINT list_members_exactly_one
    CHECK ((account_id IS NULL) <> (external IS NULL)),
  UNIQUE (list_id, account_id),
  UNIQUE (list_id, external),
  CONSTRAINT list_members_seq UNIQUE (list_id, seq)
);

-- Assigns the next per-list seq. The list row is locked first (FOR NO KEY UPDATE, so it does
-- not block the FK key-share locks of other inserts, only other seq allocators of this list):
-- concurrent adds to one list queue up instead of computing the same MAX+1.
CREATE FUNCTION rs_list_members_seq() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  PERFORM 1 FROM lists WHERE id = NEW.list_id FOR NO KEY UPDATE;
  SELECT COALESCE(MAX(seq), 0) + 1 INTO NEW.seq FROM list_members WHERE list_id = NEW.list_id;
  RETURN NEW;
END;
$$;
CREATE TRIGGER list_members_seq_ins BEFORE INSERT ON list_members
  FOR EACH ROW WHEN (NEW.seq IS NULL) EXECUTE FUNCTION rs_list_members_seq();

-- Cross-table uniqueness cannot be made race-free with EXISTS triggers: a statement keeps the
-- snapshot it acquired before waiting on a lock. This registry gives all three owner tables one
-- physical unique key, so concurrent claims are serialized by PostgreSQL's unique index.
CREATE TABLE address_owners (
  domain_id   bigint NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  local_part  text COLLATE "C" NOT NULL,
  owner_kind  text NOT NULL CHECK (owner_kind IN ('account','alias','list')),
  owner_id    bigint NOT NULL,
  PRIMARY KEY (domain_id, local_part),
  UNIQUE (owner_kind, owner_id)
);

CREATE FUNCTION rs_address_unique() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
DECLARE
  kind text := CASE TG_TABLE_NAME
    WHEN 'accounts' THEN 'account'
    WHEN 'aliases' THEN 'alias'
    ELSE 'list'
  END;
BEGIN
  IF TG_OP = 'DELETE' THEN
    DELETE FROM address_owners
    WHERE domain_id = OLD.domain_id AND local_part = OLD.local_part
      AND owner_kind = kind AND owner_id = OLD.id;
    RETURN OLD;
  END IF;

  IF TG_OP = 'UPDATE'
     AND OLD.domain_id = NEW.domain_id AND OLD.local_part = NEW.local_part THEN
    RETURN NEW;
  END IF;

  IF TG_OP = 'UPDATE' THEN
    DELETE FROM address_owners
    WHERE domain_id = OLD.domain_id AND local_part = OLD.local_part
      AND owner_kind = kind AND owner_id = OLD.id;
  END IF;

  BEGIN
    INSERT INTO address_owners(domain_id, local_part, owner_kind, owner_id)
    VALUES (NEW.domain_id, NEW.local_part, kind, NEW.id);
  EXCEPTION WHEN unique_violation THEN
    RAISE EXCEPTION 'address already used' USING ERRCODE = 'RS001';
  END;
  RETURN NEW;
END;
$$;
CREATE TRIGGER accounts_addr_unique_ins BEFORE INSERT ON accounts
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER accounts_addr_unique_upd BEFORE UPDATE OF local_part, domain_id ON accounts
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER accounts_addr_unique_del BEFORE DELETE ON accounts
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER aliases_addr_unique_ins BEFORE INSERT ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER aliases_addr_unique_upd BEFORE UPDATE OF local_part, domain_id ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER aliases_addr_unique_del BEFORE DELETE ON aliases
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER lists_addr_unique_ins BEFORE INSERT ON lists
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER lists_addr_unique_upd BEFORE UPDATE OF local_part, domain_id ON lists
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();
CREATE TRIGGER lists_addr_unique_del BEFORE DELETE ON lists
  FOR EACH ROW EXECUTE FUNCTION rs_address_unique();

-- The registry must only ever be written by rs_address_unique (trigger depth >= 2). A direct
-- INSERT/UPDATE/DELETE/TRUNCATE would let it drift from accounts/aliases/lists (an address
-- stuck forever, or an account and an alias sharing one). TRUNCATE of an owner table clears
-- that kind's registry rows. A superuser can still DISABLE these triggers; the PG8 runtime
-- role additionally gets no DML/TRUNCATE privilege on address_owners.
CREATE FUNCTION rs_address_owners_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF pg_trigger_depth() < 2 THEN
    RAISE EXCEPTION 'address_owners is maintained by triggers only' USING ERRCODE = 'RS003';
  END IF;
  IF TG_OP = 'DELETE' THEN
    RETURN OLD;
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER address_owners_guard BEFORE INSERT OR UPDATE OR DELETE ON address_owners
  FOR EACH ROW EXECUTE FUNCTION rs_address_owners_guard();
CREATE FUNCTION rs_address_owners_truncate_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  RAISE EXCEPTION 'address_owners is maintained by triggers only' USING ERRCODE = 'RS003';
END;
$$;
CREATE TRIGGER address_owners_truncate_guard BEFORE TRUNCATE ON address_owners
  FOR EACH STATEMENT EXECUTE FUNCTION rs_address_owners_truncate_guard();

CREATE FUNCTION rs_address_truncate() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  DELETE FROM address_owners
  WHERE owner_kind = CASE TG_TABLE_NAME
    WHEN 'accounts' THEN 'account' WHEN 'aliases' THEN 'alias' ELSE 'list' END;
  RETURN NULL;
END;
$$;
CREATE TRIGGER accounts_addr_truncate AFTER TRUNCATE ON accounts
  FOR EACH STATEMENT EXECUTE FUNCTION rs_address_truncate();
CREATE TRIGGER aliases_addr_truncate AFTER TRUNCATE ON aliases
  FOR EACH STATEMENT EXECUTE FUNCTION rs_address_truncate();
CREATE TRIGGER lists_addr_truncate AFTER TRUNCATE ON lists
  FOR EACH STATEMENT EXECUTE FUNCTION rs_address_truncate();

CREATE TABLE blobs (
  hash        bytea PRIMARY KEY CONSTRAINT blobs_hash_length CHECK (octet_length(hash) = 32),
  size        bigint NOT NULL CONSTRAINT blobs_size_valid CHECK (size >= 0),
  refcount    bigint NOT NULL DEFAULT 0 CONSTRAINT blobs_refcount_valid CHECK (refcount >= 0),
  zero_since  bigint CONSTRAINT blobs_zero_since_valid CHECK (zero_since IS NULL OR zero_since >= 0),
  CONSTRAINT blobs_zero_since_consistent CHECK ((refcount = 0) = (zero_since IS NOT NULL))
);

CREATE TABLE mailboxes (
  id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id       bigint NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
  next_msg_id      bigint NOT NULL DEFAULT 1 CONSTRAINT mailboxes_next_msg_id_valid CHECK (next_msg_id >= 1),
  next_change_seq  bigint NOT NULL DEFAULT 1 CONSTRAINT mailboxes_next_change_seq_valid CHECK (next_change_seq >= 1),
  uid_validity     bigint NOT NULL CONSTRAINT mailboxes_uid_validity_valid CHECK (uid_validity > 0),
  next_folder_id   bigint NOT NULL DEFAULT 1 CONSTRAINT mailboxes_next_folder_id_valid CHECK (next_folder_id >= 1)
);

CREATE TABLE folders (
  mailbox_id    bigint NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  id            bigint NOT NULL CONSTRAINT folders_id_valid CHECK (id >= 1),
  parent_id     bigint,
  name          text COLLATE "C" NOT NULL CONSTRAINT folders_name_valid CHECK (length(name) > 0 AND strpos(name, '/') = 0),
  role          text NOT NULL DEFAULT 'custom' CONSTRAINT folders_role_valid
                CHECK (role IN ('inbox','sent','drafts','trash','spam','archive','custom')),
  uid_validity  bigint NOT NULL CONSTRAINT folders_uid_validity_valid CHECK (uid_validity > 0),
  next_uid      bigint NOT NULL DEFAULT 1 CONSTRAINT folders_next_uid_valid CHECK (next_uid >= 1),
  PRIMARY KEY (mailbox_id, id),
  CONSTRAINT folders_parent_not_self CHECK (parent_id IS NULL OR parent_id <> id),
  FOREIGN KEY (mailbox_id, parent_id) REFERENCES folders(mailbox_id, id) ON DELETE NO ACTION
);
CREATE UNIQUE INDEX folders_sibling_name ON folders (mailbox_id, COALESCE(parent_id, 0), name);
CREATE UNIQUE INDEX folders_role ON folders (mailbox_id, role) WHERE role <> 'custom';

CREATE FUNCTION rs_folders_no_cycle() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF NEW.parent_id IS NULL THEN
    RETURN NEW;
  END IF;
  -- Serialize every tree mutation of this mailbox. The walk below only sees committed rows, so
  -- two concurrent reparentings (1 under 2 and 2 under 1) would otherwise both pass and commit
  -- a cycle. Lock ordering rule: the mailbox row is always locked FIRST. Statements in plpgsql
  -- take a fresh snapshot after the lock wait, so the walk sees the other session's commit.
  PERFORM 1 FROM mailboxes WHERE id = NEW.mailbox_id FOR UPDATE;
  IF EXISTS (
    WITH RECURSIVE chain(id, depth) AS (
      SELECT NEW.parent_id, 1
      UNION ALL
      SELECT f.parent_id, c.depth + 1
      FROM folders f JOIN chain c
        ON f.mailbox_id = NEW.mailbox_id AND f.id = c.id
      WHERE f.parent_id IS NOT NULL AND c.depth < 65
    )
    SELECT 1 FROM chain WHERE id = NEW.id
  ) THEN
    RAISE EXCEPTION 'folder cycle' USING ERRCODE = 'RS002';
  END IF;
  IF EXISTS (
    WITH RECURSIVE chain(id, depth) AS (
      SELECT NEW.parent_id, 1
      UNION ALL
      SELECT f.parent_id, c.depth + 1
      FROM folders f JOIN chain c
        ON f.mailbox_id = NEW.mailbox_id AND f.id = c.id
      WHERE f.parent_id IS NOT NULL AND c.depth < 65
    )
    SELECT 1 FROM chain WHERE depth >= 64
  ) THEN
    RAISE EXCEPTION 'folder tree too deep' USING ERRCODE = 'RS002';
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER folders_no_cycle BEFORE UPDATE OF parent_id ON folders
  FOR EACH ROW EXECUTE FUNCTION rs_folders_no_cycle();

CREATE TABLE messages (
  mailbox_id     bigint NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  id             bigint NOT NULL,
  folder_id      bigint NOT NULL,
  uid            bigint NOT NULL CONSTRAINT messages_uid_valid CHECK (uid >= 1),
  blob_hash      bytea NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
  size           bigint NOT NULL CONSTRAINT messages_size_valid CHECK (size > 0),
  internal_date  bigint NOT NULL,
  subject        text NOT NULL DEFAULT '',
  from_addr      text NOT NULL DEFAULT '',
  to_addrs       text NOT NULL DEFAULT '',
  message_id_hdr text COLLATE "C",
  thread_id      bigint NOT NULL,
  flags          bigint NOT NULL DEFAULT 0 CONSTRAINT messages_flags_valid CHECK (flags BETWEEN 0 AND 31),
  deleted_at     bigint,
  PRIMARY KEY (mailbox_id, id),
  FOREIGN KEY (mailbox_id, folder_id) REFERENCES folders(mailbox_id, id) ON DELETE NO ACTION,
  UNIQUE (mailbox_id, folder_id, uid)
);
CREATE INDEX messages_thread ON messages (mailbox_id, thread_id);
CREATE INDEX messages_msgid ON messages (mailbox_id, message_id_hdr);
CREATE INDEX messages_deleted_at ON messages (deleted_at) WHERE deleted_at IS NOT NULL;
-- The ON DELETE RESTRICT foreign key probes messages(blob_hash) for every blobs row GC deletes;
-- without this index each probe is a sequential scan of messages (review PG5 I1: 5.5 s vs 17 ms
-- for one GC_BATCH of 1000 rows at 100k messages).
CREATE INDEX messages_blob_hash ON messages (blob_hash);

CREATE FUNCTION rs_messages_refcount() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    UPDATE blobs SET refcount = refcount + 1 WHERE hash = NEW.blob_hash;
    RETURN NEW;
  ELSIF TG_OP = 'DELETE' THEN
    UPDATE blobs SET refcount = refcount - 1 WHERE hash = OLD.blob_hash;
    RETURN OLD;
  ELSIF OLD.blob_hash IS DISTINCT FROM NEW.blob_hash THEN
    UPDATE blobs SET refcount = refcount - 1 WHERE hash = OLD.blob_hash;
    UPDATE blobs SET refcount = refcount + 1 WHERE hash = NEW.blob_hash;
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER messages_refcount_ins AFTER INSERT ON messages
  FOR EACH ROW EXECUTE FUNCTION rs_messages_refcount();
CREATE TRIGGER messages_refcount_del AFTER DELETE ON messages
  FOR EACH ROW EXECUTE FUNCTION rs_messages_refcount();
CREATE TRIGGER messages_refcount_upd AFTER UPDATE OF blob_hash ON messages
  FOR EACH ROW EXECUTE FUNCTION rs_messages_refcount();

-- BEFORE trigger assigning NEW.zero_since: no second row version per refcount change (the
-- SQLite AFTER trigger re-updates the row). Together with blobs_zero_since_consistent this
-- makes the GC invariant a real constraint: refcount = 0 <=> zero_since is set. (Stricter than
-- SQLite, which allows `refcount>0 AND zero_since NOT NULL` and `refcount=0 AND zero_since IS NULL`
-- by direct UPDATE; the latter would hide an unreferenced blob from GC forever.)
CREATE FUNCTION rs_blobs_zero_since() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog, public AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    IF NEW.refcount = 0 THEN
      NEW.zero_since := rs_now();
    END IF;
  ELSIF NEW.refcount = 0 THEN
    IF OLD.refcount <> 0 THEN
      NEW.zero_since := rs_now();
    END IF;
  ELSE
    NEW.zero_since := NULL;
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER blobs_zero_since_ins BEFORE INSERT ON blobs
  FOR EACH ROW EXECUTE FUNCTION rs_blobs_zero_since();
CREATE TRIGGER blobs_zero_since_refcount BEFORE UPDATE OF refcount ON blobs
  FOR EACH ROW EXECUTE FUNCTION rs_blobs_zero_since();

CREATE TABLE keywords (
  mailbox_id  bigint NOT NULL,
  message_id  bigint NOT NULL,
  keyword     text COLLATE "C" NOT NULL CONSTRAINT keywords_value_valid
              CHECK (length(keyword) BETWEEN 1 AND 255 AND keyword = lower(keyword)),
  PRIMARY KEY (mailbox_id, message_id, keyword),
  FOREIGN KEY (mailbox_id, message_id) REFERENCES messages(mailbox_id, id) ON DELETE CASCADE
);

CREATE TABLE change_log (
  mailbox_id  bigint NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  seq         bigint NOT NULL CONSTRAINT change_log_seq_valid CHECK (seq >= 1),
  kind        text NOT NULL CONSTRAINT change_log_kind_valid CHECK (kind IN ('message','folder')),
  item_id     bigint NOT NULL,
  op          text NOT NULL CONSTRAINT change_log_op_valid CHECK (op IN ('create','update','destroy')),
  PRIMARY KEY (mailbox_id, seq)
);

CREATE TABLE deliveries (
  mailbox_id  bigint NOT NULL,
  delivery_id bytea NOT NULL CONSTRAINT deliveries_id_length CHECK (octet_length(delivery_id) = 32),
  message_id  bigint NOT NULL,
  blob_hash   bytea NOT NULL CONSTRAINT deliveries_hash_length CHECK (octet_length(blob_hash) = 32),
  created_at  bigint NOT NULL,
  PRIMARY KEY (mailbox_id, delivery_id),
  FOREIGN KEY (mailbox_id, message_id) REFERENCES messages(mailbox_id, id) ON DELETE CASCADE
);
CREATE INDEX deliveries_created ON deliveries (created_at);
CREATE INDEX deliveries_message ON deliveries (mailbox_id, message_id);

CREATE TABLE jobs (
  id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  kind             text COLLATE "C" NOT NULL CONSTRAINT jobs_kind_valid
                   CHECK (kind ~ '^[a-z0-9._-]{1,64}$'),
  domain_id        bigint REFERENCES domains(id) ON DELETE RESTRICT,
  state            text NOT NULL DEFAULT 'queued' CONSTRAINT jobs_state_valid
                   CHECK (state IN ('queued','running','done','failed','dead')),
  -- Keep the serialized text so the 64 KiB boundary is byte-for-byte identical to SQLite
  -- and to the public JobQueue contract. `IS JSON` provides the same validity guard.
  payload          text NOT NULL CONSTRAINT jobs_payload_valid
                   CHECK (payload IS JSON AND octet_length(payload) <= 65536),
  attempts         bigint NOT NULL DEFAULT 0 CONSTRAINT jobs_attempts_valid CHECK (attempts >= 0),
  max_attempts     bigint NOT NULL DEFAULT 5 CONSTRAINT jobs_max_attempts_valid CHECK (max_attempts BETWEEN 1 AND 20),
  run_after        bigint NOT NULL,
  lease_owner      text CONSTRAINT jobs_lease_owner_valid
                   CHECK (lease_owner IS NULL OR length(lease_owner) BETWEEN 1 AND 128),
  lease_expires_at bigint,
  epoch            bigint NOT NULL DEFAULT 0 CONSTRAINT jobs_epoch_valid CHECK (epoch >= 0),
  last_error       text CONSTRAINT jobs_last_error_valid
                   CHECK (last_error IS NULL OR octet_length(last_error) <= 512),
  created_at       bigint NOT NULL,
  updated_at       bigint NOT NULL,
  CONSTRAINT jobs_attempts_within_max CHECK (attempts <= max_attempts),
  CONSTRAINT jobs_lease_shape CHECK (
       (state = 'running' AND lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL)
    OR (state <> 'running' AND lease_owner IS NULL AND lease_expires_at IS NULL)
  )
);
CREATE INDEX jobs_state_run_after ON jobs (state, run_after);
CREATE INDEX jobs_domain_state ON jobs (domain_id, state);

CREATE TABLE capabilities (
  token_hash  bytea PRIMARY KEY CONSTRAINT capabilities_hash_length CHECK (octet_length(token_hash) = 32),
  account_id  bigint NOT NULL,
  domain_id   bigint NOT NULL,
  role        text NOT NULL CONSTRAINT capabilities_role_valid
              CHECK (role IN ('imap','jmap','dav','smtp-submit')),
  issued_at   bigint NOT NULL,
  expires_at  bigint NOT NULL,
  revoked_at  bigint CONSTRAINT capabilities_revoked_valid
              CHECK (revoked_at IS NULL OR revoked_at >= issued_at),
  CONSTRAINT capabilities_expiry_valid
    CHECK (expires_at > issued_at AND expires_at - issued_at <= 86400),
  FOREIGN KEY (account_id, domain_id) REFERENCES accounts(id, domain_id) ON DELETE CASCADE
);
CREATE INDEX capabilities_account ON capabilities (account_id, issued_at);
CREATE INDEX capabilities_expires ON capabilities (expires_at);
CREATE INDEX capabilities_revoked ON capabilities (revoked_at) WHERE revoked_at IS NOT NULL;
