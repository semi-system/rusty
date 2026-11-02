CREATE TABLE domains (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE CHECK (length(name) > 0 AND name = lower(name)),
  config      TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config)),
  created_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE plans (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE CHECK (length(name) > 0),
  config      TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config))
) STRICT;

CREATE TABLE accounts (
  id            INTEGER PRIMARY KEY,
  local_part    TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id     INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  plan_id       INTEGER REFERENCES plans(id) ON DELETE RESTRICT,   -- NULL = domain default
  password_hash TEXT,                                              -- NULL = external auth (LDAP/OIDC)
  status        TEXT NOT NULL CHECK (status IN ('active','locked','disabled')),
  config        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config)),
  created_at    INTEGER NOT NULL,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE aliases (
  id          INTEGER PRIMARY KEY,
  local_part  TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE lists (
  id          INTEGER PRIMARY KEY,
  local_part  TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE list_members (
  list_id     INTEGER NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  account_id  INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
  external    TEXT,
  CHECK ((account_id IS NULL) <> (external IS NULL)),              -- exactly one of the two
  UNIQUE (list_id, account_id),
  UNIQUE (list_id, external)
) STRICT;

-- An address (local_part, domain_id) belongs to exactly one of accounts / aliases / lists.
CREATE TRIGGER accounts_addr_unique_ins BEFORE INSERT ON accounts
BEGIN
  SELECT RAISE(ABORT, 'address already used by an alias or list')
  WHERE EXISTS (SELECT 1 FROM aliases WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM lists   WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;
CREATE TRIGGER accounts_addr_unique_upd BEFORE UPDATE ON accounts
BEGIN
  SELECT RAISE(ABORT, 'address already used by an alias or list')
  WHERE EXISTS (SELECT 1 FROM aliases WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM lists   WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;
CREATE TRIGGER aliases_addr_unique_ins BEFORE INSERT ON aliases
BEGIN
  SELECT RAISE(ABORT, 'address already used by an account or list')
  WHERE EXISTS (SELECT 1 FROM accounts WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM lists    WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;
CREATE TRIGGER aliases_addr_unique_upd BEFORE UPDATE ON aliases
BEGIN
  SELECT RAISE(ABORT, 'address already used by an account or list')
  WHERE EXISTS (SELECT 1 FROM accounts WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM lists    WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;
CREATE TRIGGER lists_addr_unique_ins BEFORE INSERT ON lists
BEGIN
  SELECT RAISE(ABORT, 'address already used by an account or alias')
  WHERE EXISTS (SELECT 1 FROM accounts WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM aliases  WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;
CREATE TRIGGER lists_addr_unique_upd BEFORE UPDATE ON lists
BEGIN
  SELECT RAISE(ABORT, 'address already used by an account or alias')
  WHERE EXISTS (SELECT 1 FROM accounts WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id)
     OR EXISTS (SELECT 1 FROM aliases  WHERE local_part = NEW.local_part AND domain_id = NEW.domain_id);
END;

CREATE TABLE blobs (
  hash      TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
  size      INTEGER NOT NULL CHECK (size >= 0),
  refcount  INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0)
) STRICT;

CREATE TABLE mailboxes (
  id               INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
  next_msg_id      INTEGER NOT NULL DEFAULT 1 CHECK (next_msg_id >= 1),
  next_change_seq  INTEGER NOT NULL DEFAULT 1 CHECK (next_change_seq >= 1),
  uid_validity     INTEGER NOT NULL CHECK (uid_validity > 0)
) STRICT;

CREATE TABLE folders (
  mailbox_id    INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  id            INTEGER NOT NULL CHECK (id >= 1),
  parent_id     INTEGER,                                           -- NULL = root
  name          TEXT NOT NULL CHECK (length(name) > 0 AND instr(name, '/') = 0),
  role          TEXT NOT NULL DEFAULT 'custom'
                CHECK (role IN ('inbox','sent','drafts','trash','spam','archive','custom')),
  uid_validity  INTEGER NOT NULL CHECK (uid_validity > 0),
  next_uid      INTEGER NOT NULL DEFAULT 1 CHECK (next_uid >= 1),
  PRIMARY KEY (mailbox_id, id),
  CHECK (parent_id IS NULL OR parent_id <> id),
  FOREIGN KEY (mailbox_id, parent_id) REFERENCES folders(mailbox_id, id) ON DELETE RESTRICT
) STRICT;
CREATE UNIQUE INDEX folders_sibling_name ON folders (mailbox_id, COALESCE(parent_id, 0), name);
CREATE UNIQUE INDEX folders_role ON folders (mailbox_id, role) WHERE role <> 'custom';

-- No cycles in the folder tree; the tree may be at most 64 levels deep.
CREATE TRIGGER folders_no_cycle BEFORE UPDATE OF parent_id ON folders
WHEN NEW.parent_id IS NOT NULL
BEGIN
  SELECT RAISE(ABORT, 'folder cycle')
  WHERE EXISTS (
    WITH RECURSIVE chain(id, depth) AS (
      SELECT NEW.parent_id, 1
      UNION ALL
      SELECT f.parent_id, c.depth + 1
      FROM folders f JOIN chain c ON f.mailbox_id = NEW.mailbox_id AND f.id = c.id
      WHERE f.parent_id IS NOT NULL AND c.depth < 65
    )
    SELECT 1 FROM chain WHERE id = NEW.id
  );
  SELECT RAISE(ABORT, 'folder tree too deep')
  WHERE EXISTS (
    WITH RECURSIVE chain(id, depth) AS (
      SELECT NEW.parent_id, 1
      UNION ALL
      SELECT f.parent_id, c.depth + 1
      FROM folders f JOIN chain c ON f.mailbox_id = NEW.mailbox_id AND f.id = c.id
      WHERE f.parent_id IS NOT NULL AND c.depth < 65
    )
    SELECT 1 FROM chain WHERE depth >= 64
  );
END;

CREATE TABLE messages (
  mailbox_id     INTEGER NOT NULL,
  id             INTEGER NOT NULL,
  folder_id      INTEGER NOT NULL,
  uid            INTEGER NOT NULL CHECK (uid >= 1),
  blob_hash      TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
  size           INTEGER NOT NULL CHECK (size > 0),
  internal_date  INTEGER NOT NULL,
  subject        TEXT NOT NULL DEFAULT '',
  from_addr      TEXT NOT NULL DEFAULT '',
  to_addrs       TEXT NOT NULL DEFAULT '',
  message_id_hdr TEXT,                                             -- NULL if message has no Message-ID
  thread_id      INTEGER NOT NULL,
  flags          INTEGER NOT NULL DEFAULT 0 CHECK (flags BETWEEN 0 AND 31),
  deleted_at     INTEGER,                                          -- NULL = not soft-deleted
  PRIMARY KEY (mailbox_id, id),
  FOREIGN KEY (mailbox_id) REFERENCES mailboxes(id) ON DELETE CASCADE,
  FOREIGN KEY (mailbox_id, folder_id) REFERENCES folders(mailbox_id, id) ON DELETE RESTRICT,
  UNIQUE (mailbox_id, folder_id, uid)
) STRICT;
CREATE INDEX messages_thread ON messages (mailbox_id, thread_id);
CREATE INDEX messages_msgid  ON messages (mailbox_id, message_id_hdr);

-- blobs.refcount is maintained only by these triggers.
CREATE TRIGGER messages_refcount_ins AFTER INSERT ON messages
BEGIN
  UPDATE blobs SET refcount = refcount + 1 WHERE hash = NEW.blob_hash;
END;
CREATE TRIGGER messages_refcount_del AFTER DELETE ON messages
BEGIN
  UPDATE blobs SET refcount = refcount - 1 WHERE hash = OLD.blob_hash;
END;
CREATE TRIGGER messages_refcount_upd AFTER UPDATE OF blob_hash ON messages
WHEN OLD.blob_hash <> NEW.blob_hash
BEGIN
  UPDATE blobs SET refcount = refcount - 1 WHERE hash = OLD.blob_hash;
  UPDATE blobs SET refcount = refcount + 1 WHERE hash = NEW.blob_hash;
END;

CREATE TABLE keywords (
  mailbox_id  INTEGER NOT NULL,
  message_id  INTEGER NOT NULL,
  keyword     TEXT NOT NULL CHECK (length(keyword) BETWEEN 1 AND 255 AND keyword = lower(keyword)),
  PRIMARY KEY (mailbox_id, message_id, keyword),
  FOREIGN KEY (mailbox_id, message_id) REFERENCES messages(mailbox_id, id) ON DELETE CASCADE
) STRICT;

CREATE TABLE change_log (
  mailbox_id  INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL CHECK (seq >= 1),
  kind        TEXT NOT NULL CHECK (kind IN ('message','folder')),
  item_id     INTEGER NOT NULL,                                    -- no FK: item may be deleted
  op          TEXT NOT NULL CHECK (op IN ('create','update','destroy')),
  PRIMARY KEY (mailbox_id, seq)
) STRICT;
