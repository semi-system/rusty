-- The relay (smarthost) of one domain, entered by its tenant administrator (admin web design, section 14.4). The password is NOT here: it is a
-- sealed secret (owner kind `domain`, purpose `relay-password`, table `sealed_secrets`). One row per domain; a domain without a row uses the
-- platform relay of `[outbound.relay]` or sends directly.
--
-- The checks repeat rusty_core::relay_policy (the store validates first; these are the second line): a tenant picks where the outbound service
-- connects, so only the four mail ports, a login never over a clear connection, implicit TLS on 465 only. The host is a name or a public IP
-- literal; whether it RESOLVES to public addresses is checked at every connection by the outbound service, which no constraint can do.
CREATE TABLE domain_relays (
  domain_id    bigint PRIMARY KEY REFERENCES domains(id) ON DELETE CASCADE,
  host         text COLLATE "C" NOT NULL CONSTRAINT domain_relays_host_valid
               CHECK (length(host) BETWEEN 3 AND 253 AND host ~ '^[a-z0-9.:-]+$'),
  port         integer NOT NULL CONSTRAINT domain_relays_port_valid CHECK (port IN (25, 465, 587, 2525)),
  tls          text COLLATE "C" NOT NULL CONSTRAINT domain_relays_tls_valid CHECK (tls IN ('starttls', 'implicit', 'none')),
  username     text COLLATE "C" CONSTRAINT domain_relays_username_valid
               CHECK (username IS NULL OR (length(username) BETWEEN 1 AND 256 AND username !~ '[[:cntrl:]]')),
  spf_include  text COLLATE "C" CONSTRAINT domain_relays_spf_valid
               CHECK (spf_include IS NULL OR (length(spf_include) BETWEEN 1 AND 253 AND spf_include ~ '^[A-Za-z0-9._-]+$')),
  check_status text COLLATE "C" CONSTRAINT domain_relays_status_valid
               CHECK (check_status IS NULL OR check_status IN ('ok', 'auth_failed', 'tls_failed', 'unreachable', 'refused_address', 'no_login_mechanism', 'unsealable')),
  checked_at   bigint,
  created_at   bigint NOT NULL,
  updated_at   bigint NOT NULL,
  CONSTRAINT domain_relays_login_needs_tls CHECK (username IS NULL OR tls <> 'none'),
  CONSTRAINT domain_relays_implicit_port CHECK (tls <> 'implicit' OR port = 465),
  CONSTRAINT domain_relays_check_pair CHECK ((check_status IS NULL) = (checked_at IS NULL))
);
