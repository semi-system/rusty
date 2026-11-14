# Postgres as the production backend

Status: Postgres is the only backend, for production (`rusty d --config FILE`), for the dev profile
(`rusty d --dev`, which names its server in `RUSTY_DEV_PG_URL`, see [local-run.md](local-run.md)) and for the tests. Message
bodies are content-addressed files on a volume (`blob_dir`), never database rows.

## 1. Configuration (`[store]`)

```toml
[store]
url = "postgres://rusty_app@db.internal:5432/rusty"   # required; NO password, NO query parameters
password_file = "/run/secrets/rusty_pg_password"      # 0600, owned by the service account
migrator_user = "rusty_migrator"                      # used by `rustyctl db migrate` only
migrator_password_file = "/run/secrets/rusty_pg_migrator_password"
pool_size = 16                             # at least (enabled listeners + 2) + 4; default 16, max 200
acquire_timeout_ms = 5000                  # wait for a pooled connection, then Busy
lock_timeout_ms = 10000                    # row-lock wait per transaction (SET LOCAL) and per-mailbox queue wait
statement_timeout_ms = 30000               # UPPER BOUND accepted for the role's statement_timeout (see 3)
idle_in_transaction_timeout_ms = 30000     # same for idle_in_transaction_session_timeout
sslmode = "verify-full"                    # default; disable | require | verify-ca | verify-full
ca_bundle = "/etc/rusty/db-ca.pem"         # PEM, when the server certificate is signed by a private CA
insecure_allow_unverified_tls = false      # explicit, loud opt-out (see 4)
```

* The password lives in a file read through the `SecretSource` (mode 0600, no symlink, owned by the service account);
  it is never part of the URL, the configuration, `Debug` output, errors or logs. `StoreConfig`/`PgSecurity`/
  `PgPassword` print `<redacted>`.
* `rusty d` and `rustyctl` open the store only through `Store::open(StoreConfig)` and the repository facades
  (`crates/rusty/src/storecfg.rs` is the one place that reads the secret and builds the config; the service-crate and
  raw-db gates keep SQL and pools inside `rusty-store`).
* The `[store]` parser is covered by `crates/rusty-core/tests/config_store.rs`, a fuzz invariant in `fuzz_config` and a
  dedicated `fuzz_pg_url` target (seeds in `fuzz/seeds/`).

## 2. Roles and privileges

Created by `deploy/db/roles.sql` (run once per database as a superuser, idempotent, plain SQL, no password in it) and
`crates/rusty-store/grants_pg.sql` (the privilege matrix, applied by `rustyctl db migrate` after the migrations).

| Role | Purpose | Rights |
|---|---|---|
| `rusty_migrator` | `rustyctl db migrate` only | owns the schema and every object; `CREATE` on schema `public`; the only role with DDL |
| `rusty_app` | the daemon and `rustyctl` | `SELECT/INSERT/UPDATE/DELETE` on every table; **no** `TRUNCATE`, `TRIGGER`, `REFERENCES`, DDL, `TEMP`; `SELECT` only on `address_owners` and `_sqlx_migrations` |
| `rusty_ro` | optional reporting | `SELECT` on every table except `accounts.password_hash`; nothing else |

Roles are created `NOLOGIN`: the operator enables each login on purpose (`ALTER ROLE .. LOGIN PASSWORD ..`, or a
certificate rule in `pg_hba.conf`). `PUBLIC` loses `CONNECT`/`TEMP` on the database and everything on schema `public`.

### The address registry (PG8 decision, was open)

`address_owners` gives accounts, aliases and lists one physical unique key (cross-table address uniqueness without a
race). It must never be written except by the three owner tables' triggers.

Decision: **`SECURITY DEFINER` trigger functions plus a privilege revoke**, not privileges alone:

* `rs_address_unique()` and `rs_address_truncate()` (the only writers) are `SECURITY DEFINER`, owned by
  `rusty_migrator` (migration `0016`), with `search_path = pg_catalog, public, pg_temp` (`pg_temp` LAST: otherwise a
  temporary table named like a registry table would shadow it inside a definer function).
* `rusty_app` has `SELECT` only on `address_owners`. A direct `INSERT/UPDATE/DELETE/TRUNCATE` is refused (`42501`);
  the pre-existing guard trigger (`pg_trigger_depth() >= 2`) and TRUNCATE guard remain as a second line.
* Why not privileges alone: with `SECURITY INVOKER` triggers the revoke would also break legitimate account creation.
  Why not make everything definer: every other trigger function only reads or checks, needs no extra rights, and a
  definer function is attack surface. A test asserts that exactly these two functions are definers.

### Threat model

| Threat | Control |
|---|---|
| SQL injection or a logic bug in the daemon (a compromised `rusty_app` session) | no DDL, no `TRUNCATE`, no write on the registry or the migration history, no `TEMP`, no `CREATE` on `public` (tests: `pg_roles.rs`); every statement is parameterised and lives in `rusty-store` (raw-db gate) |
| Stolen `rusty_app` password | cannot read `rusty_ro` data it does not already have, cannot change the schema, cannot forge the registry; domain/tenant isolation is enforced by the repositories and unchanged; rotate the password file and restart |
| Stolen migrator password | full control of the schema. It is used only by an operator running `db migrate`, is a different file, and should not be present on the daemon host between migrations |
| Definer-function hijack (search_path / temp-table shadowing) | pinned `search_path` with `pg_temp` last; the runtime role has no `CREATE` and no `TEMP`; a catalog test asserts the pinning |
| Daemon running with a more powerful role by mistake (superuser, owner, `CREATEROLE`) | start-up `verify_role` check refuses: superuser/`CREATEROLE`/`CREATEDB`/`REPLICATION`/`BYPASSRLS`, `CREATE` on `public`, table ownership or `TRUNCATE`/`TRIGGER`/`REFERENCES`, write on the registry or `_sqlx_migrations`, missing/too long role timeouts |
| Schema drift (daemon older/newer than the database, an edited migration) | the daemon never migrates on connect and refuses to run unless the applied migrations equal the embedded ones (versions and checksums); errors: not initialised / older / newer / inconsistent |
| Credential leakage | password only in a 0600 file via `SecretSource`; redacted `Debug`; fixed error texts; sanitiser test with a wrong password |
| Eavesdropping/MITM on the database link | see 4 |
| Reporting role reads password hashes | column-level grant excludes `accounts.password_hash` |
| Resource exhaustion by one hot mailbox or a slow statement | per-mailbox queue (5), role-level `statement_timeout`/`idle_in_transaction_session_timeout`, `lock_timeout`, bounded pool; all map to `Busy` (SMTP `451 4.3.0`, IMAP `NO [UNAVAILABLE]`) |

Not defended: a superuser (can disable triggers); a compromised migrator; a malicious DBA. Row-level security by
mailbox was evaluated in PG8 and stays deferred (it needs `SET LOCAL` per transaction and does not fit the
repository shape; tenant isolation is enforced in the repositories and by foreign keys).

## 3. Timeouts are properties of the role

`statement_timeout` and `idle_in_transaction_session_timeout` are set with `ALTER ROLE .. SET` (`roles.sql`: `rusty_app`
30 s, `rusty_ro` 60 s, `rusty_migrator` 15 min) and are **not** issued as `SET` from the application any more (the old
`after_connect` `set_config` is gone): nothing in `rusty-store` keeps session state, so the code runs unchanged behind a
transaction-pooling proxy (PgBouncer, plan task PG8a). `lock_timeout` is applied per transaction with `set_config(..,
true)` (= `SET LOCAL`). The daemon only verifies: with `[store] statement_timeout_ms` / `idle_in_transaction_timeout_ms`
as the largest acceptable value, a role with no timeout (0) or a longer one makes the start-up fail.

## 4. TLS to the database

* Default and production value: `sslmode = "verify-full"` (certificate chain and host name). A private CA goes in
  `ca_bundle` (PEM, up to 1 MiB); with a CA bundle only that CA is trusted.
* The policy is enforced in two layers: configuration validation (`rusty-core`) and, below every caller, in
  `rusty_store::PgStore::connect_with` (tests: `config_store.rs`, `pg_roles.rs::tls_policy_*`).
  * Non-loopback host: anything but `verify-full` is refused unless `insecure_allow_unverified_tls = true`, which
    starts with a warning in the log on every start.
  * Loopback host (127.0.0.0/8, `::1`, `localhost`, a Unix socket path): `require`, `verify-ca`, `verify-full` accepted;
    `disable` only in the dev profile.
* `verify-ca` is treated as unverified for a remote host (it does not check the name).
* TLS uses rustls with the `ring` provider (no OpenSSL; `deny.toml` bans it).
* `scripts`: the acceptance run (`BACKEND=postgres tools/acceptance/local-run.sh`) connects with `verify-full` and a
  generated CA to a TLS-enabled container, which is the end-to-end proof of the verified path.

## 5. Pool and back-pressure

* Pool size floor: `services + 4`, where `services` = enabled listeners + the GC ticker + the outbound worker (default
  config: 5 + 2 + 4 = 11; default `pool_size` 16). `check_startup` refuses a smaller `pool_size`.
* Per-mailbox in-process queue (`crates/rusty-store/src/pg_queue.rs`): every mailbox write takes a permit **before** it
  asks the pool for a connection, so writers of a hot mailbox wait in memory instead of each holding a connection while
  they wait for the row lock. A fixed array of 4096 FIFO semaphores indexed by a hash of the mailbox id (bounded memory
  by construction, nothing to reap; two mailboxes that share a stripe only serialise against each other; a writer
  holds one permit, so no deadlock). Cancel-safe; the wait is bounded by `lock_timeout_ms`, then `Busy`.
* `Error::Busy` end to end (tests `rusty-smtp/tests/busy_e2e.rs`, `rusty-imap/tests/busy_e2e.rs`): a
  locked database and an exhausted pool answer SMTP `451 4.3.0` / IMAP `NO [UNAVAILABLE]` within a bound,
  nothing is stored, and the same message is accepted once the database is free (idempotent `DeliveryId`).

## 5a. Cross-process notifications (`LISTEN` / `NOTIFY`)

With one process per service, a message delivered by one process must wake the IDLE sessions of another, and a job queued by
submission must wake the outbound worker. The database announces both, at commit (migration 0017):

| Channel | Payload | Sent when | Heard by |
|---|---|---|---|
| `rusty_mailbox` | `<mailbox id>:<newest seq>` | rows are inserted into `change_log` (one notification per mailbox and statement) | IMAP IDLE (`MailboxHub`) |
| `rusty_jobs` | empty | rows are inserted into `jobs` | the outbound worker |

* **One extra connection per process.** Every process that runs the change feed keeps ONE dedicated connection that
  `LISTEN`s. It is not part of `pool_size` (it has its own pool of one) and it uses the same host, role, credentials and TLS
  policy as the store. Count it in `max_connections` of the server: one per running process. `LISTEN` needs a session, so
  behind PgBouncer in transaction mode this connection must go to the server directly (or to a pooler in session mode).
  No extra privilege is needed.
* **A notification is a hint, never the record.** Postgres keeps nothing for a listener that is away. After every
  (re)subscription the feed reports a resync: IDLE sessions look at their change log again and the worker at the queue. Both
  also keep their timers (IDLE polls the change log every 30 s, the worker polls the queue every 2 s), so a lost notification
  costs latency, never mail. A lost connection is retried with a delay that grows from 0.5 s to 30 s and is logged as
  `change feed: connection lost, retrying`.
* **Rolled-back work is never announced** (`NOTIFY` is delivered at commit).
* Tests: `rusty-store/tests/pg_notify.rs` (the triggers), `pg_change_feed.rs` (listening, reconnect, stop),
  `rusty-imap/tests/cross_process_idle.rs` (two contexts with separate hubs, including a delivery made while the feed was
  down), `rusty-smtp/tests/outbound_worker.rs` (a sleeping worker woken by a job another process queued).

## 6. Migrations

* `rustyctl db migrate [--without-roles]` (migrator credentials from `[store]`, connects directly to Postgres, never
  through a pooler): applies pending migrations, then `grants_pg.sql`. Idempotent. Refuses to run without the roles
  initialised in the database unless `--without-roles` (throw-away databases), refuses a runtime role, and refuses a
  database that is newer or inconsistent.
* `rustyctl db status`: `current | uninitialised | behind | ahead | dirty | mismatch`, applied/expected/pending
  versions; exit code 0 only when current.
* Upgrade procedure: stop (or drain) the daemons, `rustyctl db migrate`, start the new daemons. A new daemon against an
  old schema, or an old daemon against a newer schema, refuses to start with a clear error.

### Migration numbering (the one table)

The migrations are in `crates/rusty-store/migrations`. `0001_init` is the consolidated base schema (it replaced the
numbers `0001..0009` of the former SQLite history, which no longer exists); numbers `0002..0009` are forbidden (a file
there would be applied after an already applied `0010` on existing databases). CI (`scripts/check-pg-migration-gap.sh`)
fails if the gap is filled. Applied migrations are frozen once released.

| Number | Content |
|---|---|
| 0001 | base schema (domains, accounts, mailboxes, folders, messages, change log, blobs, deliveries, jobs, capabilities, list member seq, blob index) |
| 0010 | domain status |
| 0011 | tenant rules |
| 0012 | folder subscribed |
| 0013 | jobs enqueue (U5) |
| 0014, 0015 | DNS state tables (U10) |
| 0016 | address registry `SECURITY DEFINER` |
| 0017 | `NOTIFY` triggers on `change_log` and `jobs` (cross-process notifications, section 5a) |

A new migration takes the next number.

## 7. Operations

* Backup: `pg_dump`/`pg_basebackup` first, then the blob directory (blobs are content-addressed and immutable, so a
  blob newer than the dump is harmless; the reverse is not). `rustyctl backup` learns this in U9.
* GC singleton is still a local-volume file lock (one GC host per blob volume). The lease-row design is backlog.
* PgBouncer (transaction pooling) is supported by construction (no session state) and tested in plan task PG8a, not
  here. `db migrate` must connect directly.
* The dev compose file (`deploy/dev/rusty.compose.yaml`) starts a digest-pinned Postgres 18, a one-shot `db-init`
  (roles.sql and role logins), a one-shot `migrate` and then the daemon as `rusty_app`. Its database link is not
  TLS-verified (marked, warned about at every start).

## 8. Known limits and deferred

* Passwords are held in memory as `String` (no zeroizing crate in the tree).
* No RLS, no PgBouncer test, no GC lease row, no PG7 benchmark, `jsonb`-vs-text decision: backlog in the PG plan.
* `rusty_ro` is created and granted but nothing in Rusty uses it yet.
