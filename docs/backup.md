# Backup and restore

A Rusty installation is **a database and a blob store** (plus configuration). Both are needed; neither alone restores mail.

```
scripts/backup.sh  --out /backup/2026-11-15 --blobs /var/lib/rusty/blobs --config-dir /etc/rusty [--include-secrets /etc/rusty/keys]
scripts/restore.sh --from /backup/2026-11-15 --blobs /var/lib/rusty/blobs --config-dir /etc/rusty     # into an EMPTY database and an EMPTY blob directory
rustyctl --config /etc/rusty/rusty.toml blobs verify                                                  # every blob the database references must have its file
```

* **Order is the rule.** `backup.sh` dumps the database **first** (`pg_dump`, one consistent snapshot) and copies the blobs **after**. Blobs are content-addressed and immutable, so a blob newer
  than the dump is harmless (garbage collection removes it later); the reverse would leave messages without their file. The `MANIFEST` records the order and the times.
* The backup directory is mode 0700, must not exist or must be empty, and carries `SHA256SUMS` of everything. `restore.sh` verifies them **before touching anything**, and refuses a non-empty
  database or blob directory: a restore never merges into, or overwrites, live data.
* The database is reached with the usual `PG*` variables; use the owner or a superuser (`rusty_ro` cannot read the sealed secrets, by design). The dump holds no owners and no grants: create the roles
  (`deploy/db/roles.sql`), restore, then run `rustyctl db migrate` to re-apply the privileges. `PG_DUMP`, `PG_RESTORE` and `PSQL` override the commands (for example to go through `docker exec`).
* **Secrets are not copied unless named** (`--include-secrets DIR`: the unsealing key, DKIM key files; copied with 0600 files). Without the unsealing key the sealed DKIM keys and relay passwords in the
  database cannot be opened: **back the key up separately and test that you can**. TLS keys at the edge are the edge's own (ACME renews them; see `docs/acme.md`).
* **`rustyctl blobs verify`** reads the database and the blob store and reports, per the check: how many blobs the database references, how many files exist, how many files nothing references
  (harmless), and the hashes of referenced blobs with **no file** (data that is gone). Exit status 1 when one is missing or a shard cannot be read. It changes nothing.

`scripts/tests/backup-restore.sh` (run by CI) proves the round trip against a throw-away Postgres: rows and blob files come back identical, the database is dumped before the blobs, and every refusal
(non-empty out directory, damaged backup, non-empty database, non-empty blob directory, a directory that is not a blob root) leaves the target untouched.

Not covered: continuous archiving / point-in-time recovery (`pg_basebackup` with WAL archiving is the PostgreSQL way and is compatible with this layout), and S3 blob stores (when blobs move to S3,
versioning or replication of the bucket replaces the file copy; `blobs verify` will need an S3 reader).
