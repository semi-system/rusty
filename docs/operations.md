# Operations: upgrade, rollback and runbooks (standalone stack)

Everything here was run on the LAN test server on 2026-11-16 unless it says otherwise. The commands are for the stack `deploy/standalone` installs; run them from that directory.

## Names and the commands you need

* The database allows its clients **by container name** (`docker.md`), so a one-off container must be given the name the allow-list has: `docker compose run --rm --name rusty-ctl-1 ctl ...` for `rustyctl`,
  `--name rusty-db-init-1` for the job that makes the roles, `--name rusty-migrate-1` for the migration job. A random name is refused by the database.
* Is everything up? `docker compose ps` (all `healthy`), and the **Monitor** page of the admin web (every service `Running` with the version you expect, the outbound queue, the storage and DNS figures). With
  the `monitoring` profile the same figures are in Prometheus (`design-metrics.md`).
* Is the schema current? `docker compose run --rm --name rusty-migrate-1 migrate --config /etc/rusty/migrate.toml db status` prints `schema: current (applied N, expected N, pending [])`.
* Is the mail intact? `docker compose run --rm --name rusty-ctl-1 ctl blobs verify` (every referenced blob exists), and send a test message through the edge.

## Upgrade

1. Read what changed, and the new migrations (`postgres-production.md`, "Migration numbering"). A migration must keep the **previous** release working while the new one rolls out (add first, remove in a
   later release): that is the rule that makes an upgrade a short restart instead of an outage.
2. **Take a backup first** (`backup.sh --out DIR --with-keys`, `backup.md`). After a migration it is the only way back (see Rollback).
3. Verify the images you are about to run (`docker.md`, the `cosign verify` command) and pin a release tag in production: `./setup.sh ...the same arguments... --image-tag 1.2.3` pulls the images and rewrites the
   configuration, keeping the secrets and keys that exist.
4. `docker compose up -d`. The migration job runs first (the services wait for it), then every changed service is recreated. Expect a short interruption of the mail ports while they restart: senders retry,
   mail clients reconnect.
5. Check: `docker compose ps`, `db status` (current), the Monitor page, one message in and one out.

The upgrade drill (a stack with 59 messages, 5 accounts, 59 blobs, from `sha-6b2523c` to `sha-ce996d1`, which applies migration 0027): everything healthy after `up -d`, the same data, `db status` current, a
heartbeat row for each of the six services.

## Rollback

* **A release that applied no migration** (`db status` says the same `applied N` for the old and the new release): set the old tag, `./setup.sh ... --image-tag OLD`, `docker compose up -d`.
* **A release that applied a migration**: the old images cannot run on the new schema, and there is no down migration. Putting only the old images back does **not work and does not damage anything**: the migration job
  stops with `database schema history is inconsistent or newer than this binary: refusing to migrate`, `docker compose up -d` reports that `migrate` did not complete, and the services do not come up (in the
  drill `docker compose ps` was empty afterwards): the stack is down, the database and the blobs are untouched. You can go forward again (the new tag) or take the way back, which is to restore the backup of step 2 of the upgrade into new volumes with the old images:

  ```sh
  ./setup.sh ... --image-tag OLD
  docker compose down -v
  ./restore.sh --from /backup/before-the-upgrade --with-keys
  docker compose up -d
  ```

  Mail accepted after that backup is **lost** by this path. To lose less, stop the inbound ports first (`docker compose stop smtp-in`; senders retry for days) and take a second backup of the new state
  before restoring, in case you want something out of it. The drill did this and ended with the same 59 messages and the same 38 messages over IMAP as before the upgrade.

## Version rules the services enforce

| Situation | What happens |
|---|---|
| A new binary meets an older schema | refuses to start: run the migration (`up -d` does it first) |
| An old binary meets a newer schema | refuses to start (and the migration job refuses to run): restore, or run a newer release |
| A migration that failed half way, or one that was edited after it was applied | everything refuses to start; `db status` says `dirty` or `mismatch` |

## Runbooks

**A service restarts in a loop.** `docker compose logs SERVICE --tail 50`. The usual causes: the configuration was rewritten by `setup.sh` with different arguments than the installation has (run it again with the
right ones); the database refused the container (`docker compose logs db`: `pg_hba.conf rejects` means a replica number above `--max-replicas`, or a name that is not on the list); the key-value store is
configured but unreachable (a service whose store is unreachable at start does not start, on purpose); the schema does not match the image (above).

**The outbound queue grows, or the oldest due message is old.** Monitor page, then `docker compose logs outbound`. A growing queue with a live worker is usually the destination refusing (greylisting,
a blocklist, a relay that is down): `rustyctl` shows the relay state per domain, the DNS page shows the records. A due message that nobody takes means no outbound instance beats: start it. Given-up messages
(`dead`) have bounced to the sender.

**Incoming mail is deferred with 451.** The virus scanner is down: the inbound service holds mail while clamd cannot be asked (`clamd_on_error` defaults to `tempfail`; rspamd's defaults to `accept`, so a
spam scorer that is down lets mail through marked as not scanned). `docker compose ps clamd freshclam`: clamd needs a few minutes after a start while it loads the signatures. The scan result counters and the
`RustyScannerFailing` alert show it.

**A tenant's mail is refused or fails for a storage reason.** Monitor, "Storage": a domain whose store fails its check. The domain's storage page has the settings and the check; keys are never shown. The blob
store of a tenant is the tenant's bucket: its availability is the bucket's.

**The database refuses connections ("too many clients").** Every replica opens its own pool (`[store] max_connections`): the sum over all replicas must stay below the server's `max_connections`. Lower the
pools or put PgBouncer in front (`postgres-production.md`).

**Disk full.** `docker system df`, then the volumes: `pgdata` (the database), `mail-data` (blobs, when they are on disk), `clamdb`, `promdata`. Garbage collection removes unreferenced blobs after the grace
period; it cannot remove anything the database still references.

**The sealing key is lost.** Without it the sealed DKIM keys, relay passwords and S3 keys in the database cannot be opened, and there is no way to recover them: new DKIM keys, the relay passwords and the
tenants' S3 keys must be entered again. That is why `backup.sh --with-keys` exists and why the copy of the keys is kept apart from the rest.

**Certificates.** With ACME the edge renews them itself over HTTP-01, which needs port 80 reachable from the internet; `docker compose logs edge`. In self-signed mode they are valid 825 days: delete
`state/edge/certs/*` and `state/tls/*` and run `setup.sh` again.

**Valkey is down** (with the `valkey` profile). Limits and the login throttle fall back to each process's own counters; the admin web's sessions are kept in it, so signing in again is needed. A service
that is started while the store is unreachable does not start: bring the store up first (the services wait for its health check in the stack).

**Rotate a database password.** Tested on 2026-11-16 for the IMAP service; the other roles are the same. Delete its secret file (`state/secrets/pg_imap`, owned by the service user: use a throw-away root
container such as `docker run --rm --user 0 -v STATE/secrets:/s --entrypoint rm IMAGE -f /s/pg_imap`), run `./setup.sh ...` (it makes the missing secret and keeps the others), `docker rm -f rusty-db-init-1;
docker compose run --rm --name rusty-db-init-1 db-init` (it sets the roles' passwords from the secrets), then `docker compose up -d --force-recreate imap`. The drill: the new secret took effect, the service was
healthy again 25 seconds later, logins worked, and the database logged no failed authentication.
