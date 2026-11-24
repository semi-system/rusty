-- Calendars and contacts (docs/design-caldav-carddav.md): collections (a calendar or an address book of an account) and the objects in them (an event, a contact). An object is kept as it was uploaded; the columns beside
-- the text are what the reports filter on. The revision of a collection counts its changes: it is the ctag and the base of the sync token; a deleted object leaves a tombstone so that a client can be told.
CREATE TABLE dav_collections (
  id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id      bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  kind            text NOT NULL CONSTRAINT dav_collections_kind CHECK (kind IN ('calendar', 'addressbook')),
  name            text COLLATE "C" NOT NULL CONSTRAINT dav_collections_name CHECK (name ~ '^[A-Za-z0-9._-]{1,128}$'),
  display_name    text NOT NULL DEFAULT '' CONSTRAINT dav_collections_display CHECK (length(display_name) <= 256 AND display_name !~ '[[:cntrl:]]'),
  description     text NOT NULL DEFAULT '' CONSTRAINT dav_collections_description CHECK (length(description) <= 2048),
  color           text NOT NULL DEFAULT '' CONSTRAINT dav_collections_color CHECK (color ~ '^(#[0-9A-Fa-f]{6}([0-9A-Fa-f]{2})?)?$'),
  revision        bigint NOT NULL DEFAULT 0,
  -- Tombstones up to this revision were collected: a sync token below it cannot be answered with a delta.
  tombstone_floor bigint NOT NULL DEFAULT 0,
  created_at      bigint NOT NULL,
  updated_at      bigint NOT NULL,
  UNIQUE (account_id, kind, name)
);

CREATE TABLE dav_objects (
  collection_id bigint NOT NULL REFERENCES dav_collections(id) ON DELETE CASCADE,
  name          text COLLATE "C" NOT NULL CONSTRAINT dav_objects_name CHECK (name ~ '^[A-Za-z0-9._-]{1,128}$'),
  uid           text COLLATE "C" NOT NULL CONSTRAINT dav_objects_uid CHECK (length(uid) BETWEEN 1 AND 255),
  etag          text NOT NULL,
  body          text COLLATE "C" NOT NULL CONSTRAINT dav_objects_body CHECK (length(body) <= 1048576),
  size          integer NOT NULL,
  -- VEVENT, VTODO, VJOURNAL or VCARD.
  component     text NOT NULL CONSTRAINT dav_objects_component CHECK (component IN ('VEVENT', 'VTODO', 'VJOURNAL', 'VCARD')),
  -- The outer bounds of an event, in seconds since the epoch; null: not bounded (a rule without an end has no end).
  starts_at     bigint,
  ends_at       bigint,
  recurring     boolean NOT NULL DEFAULT false,
  revision      bigint NOT NULL,
  updated_at    bigint NOT NULL,
  PRIMARY KEY (collection_id, name),
  UNIQUE (collection_id, uid)
);
CREATE INDEX dav_objects_range ON dav_objects (collection_id, starts_at, ends_at);
CREATE INDEX dav_objects_revision ON dav_objects (collection_id, revision);

CREATE TABLE dav_tombstones (
  collection_id bigint NOT NULL REFERENCES dav_collections(id) ON DELETE CASCADE,
  name          text COLLATE "C" NOT NULL,
  revision      bigint NOT NULL,
  deleted_at    bigint NOT NULL,
  PRIMARY KEY (collection_id, name)
);
CREATE INDEX dav_tombstones_age ON dav_tombstones (deleted_at);

-- Additive: a binary of the previous schema never reads the tables.
INSERT INTO schema_compat (version, min_binary) VALUES (58, 57);
