-- Platform-wide switches the operator sets in the admin web (docs/design-protocols.md): which optional protocols the mail services answer. A key with no row has its default (on),
-- so a fresh installation needs no row. The services read it (cached for a few seconds), the admin web writes it. The keys are a closed list: a new protocol adds its key here.
CREATE TABLE platform_settings (
  key        text COLLATE "C" PRIMARY KEY CONSTRAINT platform_settings_key_valid CHECK (key IN ('protocol_pop3')),
  enabled    boolean NOT NULL,
  updated_at bigint NOT NULL CONSTRAINT platform_settings_updated_valid CHECK (updated_at >= 0)
);
