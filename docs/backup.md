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

## The standalone stack (Docker Compose)

`deploy/standalone/backup.sh` and `restore.sh` do the same for the stack `setup.sh` installs (the commands above are for a database and a blob directory you run yourself).

```sh
./backup.sh --out /backup/2026-11-16 --with-keys        # the services keep running
# ...a new host, or the same one after `docker compose down -v`:
./setup.sh ...the same arguments...                      # then
./restore.sh --from /backup/2026-11-16 --with-keys
docker compose up -d && docker compose run --rm --name rusty-ctl-1 ctl blobs verify
```

* The database is dumped first (`pg_dump -Fc`, one consistent snapshot) and the blob volume (`mail-data`) copied after, for the reason given above. The files are written by whoever runs the script, mode 0600,
  with `SHA256SUMS` and a `MANIFEST`.
* `--with-keys` adds the sealing key pair. **Without it the sealed DKIM keys, relay passwords and S3 keys in the database can never be opened again**; keep that copy apart from the rest.
* `restore.sh` checks `SHA256SUMS` first, makes the roles (`db-init`), refuses a database that already has tables and a blob volume that is not empty, restores the dump **as the schema owner**
  (`rusty_migrator`, so the `SECURITY DEFINER` functions have the right owner), and unpacks the blobs. The next `up` runs the migration job, which applies the privileges again.
* A blob store on S3 is not copied: the bucket's own versioning or replication does that. After any restore `rustyctl blobs verify` reads every store and reports a referenced object that is missing.
* The names matter: `docker compose run` gives containers random names and the database allows its clients by name (`docker.md`), so the scripts and the commands above pass `--name`.

**The drill (2026-11-16, the LAN test server).** A stack with 49 messages, 5 accounts and one domain was backed up, every volume was removed (`down -v`), the stack was restored from the backup alone and
started: the same 49 messages, accounts and blobs came back, IMAP showed the same 28 messages in the mailbox of the test account, `blobs verify` found 49 of 49 blobs, and new mail was accepted and
delivered. Two defects of the first draft of the scripts showed up only in this run (backup files owned by root; the role-creating job started under a random container name and was refused by the
database) and are fixed. Repeat the drill after any change to the stack, on a copy.

## The swarm (`rusty-swarm`)

```sh
./rusty-swarm backup [--with-keys] [--with-blobs] [--keep N]   # a dump of the database in STATE_DIR/backups/<time>/ (and, with --with-blobs, the RustFS volume of the stack)
./rusty-swarm backup verify [FILE]                              # the drill: the newest dump (or FILE) is restored into a scratch database next to the real one, read, and dropped
./rusty-swarm backup install-timer                              # every night at 02:30: backup --keep BACKUP_KEEP (default 14), then verify; needs systemd and a Manager reached as `local`
```

* `backup verify` proves what matters: the dump **restores** and its tables **read** (migration count and newest version, domains, accounts, messages are printed, next to the live database's migration
  state). The real database is not touched; the database login needs `CREATEDB` (the stack's own PostgreSQL has it). A backup that is not verified is a hope.
* `--with-blobs` copies the RustFS volume of the stack (a root container with one capability, `DAC_READ_SEARCH`, reading the volume read-only). It is a copy of live files: it holds the mail that existed
  when it began, and the database dump is the exactly consistent part, so restore the dump and the blobs together and run `rustyctl blobs verify` (missing files are reported, extra ones are harmless).
  With an S3 of your own, replicate the bucket there (versioning or cross-site replication).
* `--keep N` removes the oldest backups of this tool (directories named by a time stamp) beyond the newest N. Copy the directory off the machine: a backup on the disk it protects is one disk failure from nothing.
* The sealing key (`STATE_DIR/keys`) is **not** in the dump unless `--with-keys`: without it the sealed DKIM keys and relay passwords in the dump cannot be read. Keep a copy elsewhere and test it.

**The way back** (a new Manager, or after a loss): the same `swarm.env` and `STATE_DIR` (the keys!), the kit of the release, then

```sh
./rusty-swarm prepare secrets && ./rusty-swarm prepare configs
./rusty-swarm restore STATE_DIR/backups/<time>/rusty.dump      # the services stopped (docker stack rm rusty) while it runs
./rusty-swarm prepare db && ./rusty-swarm migrate && ./rusty-swarm deploy
# the mail: unpack blobs.tar.gz into the RustFS volume (or restore your bucket), then
./rusty-swarm verify
```
