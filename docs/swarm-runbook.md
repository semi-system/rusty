# Running the swarm: checks, upgrade, rollback, keys and what to do when it breaks

For the stack `deploy/swarm/rusty-swarm` makes (`swarm.md`). `operations.md` has the same for the standalone (Compose) stack. Every command runs from the kit's directory with `--env swarm.env`. Each part says
whether it was drilled on the test VM (a one-node swarm) and when.

## Every day (or let the alerts tell you: `MONITORING=1`, `operations.md`)

* `./rusty-swarm verify`: the greeting of ports 25, 587, 143 and 110 through the edge (and of the Prost pilot's ports).
* `docker service ls`: every service at its replicas. The **Monitor** page of the admin web: every service `Running` with the version you expect, the outbound queue, the storage and DNS figures.
* The newest backup: `ls STATE_DIR/backups` (a timer makes one every night: `backup install-timer`), and that it is also **off the machine**.

## Upgrade (drilled 2026-11-22 and 2026-11-23: 0031 to 0047, two image tags)

```sh
tar xzf rusty-deploy-<release>.tar.gz && cd rusty-deploy-<release>
# swarm.env: IMAGE_TAG is the release (or the kit's images.lock names the images by digest)
./rusty-swarm --env ~/swarm.env upgrade        # backup, secrets, configs, the migration job on the new image, then a rolling update
./rusty-swarm --env ~/swarm.env verify
```

* The migration runs first and **only adds** (a migration the previous release cannot read is refused by the repository's lints), so the old services keep running while the new ones roll out; a service that does not
  come up is rolled back by the swarm by itself (`failure_action: rollback`).
* While it rolls, `docker service ls` shows `3/2` for a minute: a new task is started before the old one stops. A `verify` in that minute can fail once; run it again.
* A key added to a configuration that an older image refuses (`ADMIN_PROXY`, for one) is switched on **after** the image that knows it is deployed: deploy the new images first, set the switch second.
* After the upgrade the timer of `backup install-timer` and the kit directory: the timer's unit names the directory of the kit that installed it; run `backup install-timer` again from the new kit.

## Rollback

* **A release that applied no migration** (the same `applied N` for both): set the old `IMAGE_TAG` and run `deploy`.
* **A release that applied a migration**: the old images refuse a newer schema (the migration job says so and the services do not start; the database is untouched). Go forward again, or restore the backup the
  upgrade took first (`backup.md`, "The swarm"): `docker stack rm rusty`, `restore FILE`, `prepare db`, `migrate`, old `IMAGE_TAG`, `deploy`. Mail accepted after that backup is lost by this path: stop
  `smtp-in` first (`docker service scale rusty_smtp-in=0`; senders retry for days) and take a second backup of the new state before restoring.
* The edge only: `EDGE=haproxy` and `deploy` puts HAProxy back as the edge (the Prost pilot, if it was there, goes).

## Keys and passwords

* **DKIM keys**: rotate from the DKIM section of a domain in the admin web: a new selector is published, the old key retires itself after seven days (`design-dkim-rotation.md`).
* **The sealing key** (`STATE_DIR/keys/unseal.key`, and `seal.pub`): there is no rotation of it, and **without it the sealed DKIM keys, relay passwords and tenants' S3 keys in the database cannot be read**.
  Keep a copy off the machine (`backup --with-keys` includes it; keep that backup apart from the dump) and prove once that you can read the copy.
* **The key of the second factor** (`STATE_DIR/keys/webmail_totp.key`, made with `WEBMAIL_HOST` set): the webmail seals the two-step sign-in secrets of its people to it. There is no rotation yet; without it, people who turned the second factor on cannot sign in
  (an account without it is not affected). It is in `backup --with-keys` like the sealing key. A person who lost their device uses a recovery code; if those are gone too, an administrator removes the row (`DELETE FROM totp WHERE account_id = ...`, a `rustyctl` command is to come).
* **A role's database password** (not drilled in the swarm; the standalone stack's drill is in `operations.md`): delete `STATE_DIR/secrets/pg_<role>`, then `prepare secrets` (a new random password and a new
  version of the Docker secret), `prepare configs`, `prepare db` (sets the role's password in the database; the running tasks, which still hold the old one, fail to open new connections from here),
  and at once `deploy` (a rolling update onto the new secret). Do one role at a time, and check the Monitor page after.
* **Edge certificates** (`TLS_MODE=acme`): renewed by the edge by itself; `certs sync` carries the mail host's certificate to the mail services (`certs install-timer` runs it every six hours).

## When it breaks

* **A service restarts in a loop**: `./rusty-swarm logs SERVICE 80`. The usual causes: the database refused it (the allow-list is by service name), a configuration key that its image does not know (an older
  image against a newer kit: the log says `configuration is not valid`), the schema is not the image's (`docker service logs rusty_migrate-...`).
* **The outbound queue grows**: Monitor, then `logs outbound`. A live worker and a growing queue means the destination refuses (greylisting, a blocklist, a relay that is down); the DNS page shows our records.
* **Incoming mail is deferred with 451**: the virus scanner is down (`docker service ps rusty_clamd`; it needs minutes to load its signatures after a start).
* **"Too many connections" from one client address**: the services limit connections per client address (`max_connections_per_ip`), and so does the Prost edge (`PROST_MAX_PER_IP`, `PROST_CONN_RATE`).
  Many users behind one address (an office NAT) need the limits raised.
* **The edge**: `logs edge` (HAProxy) or `edge status` and `logs edge-prost` (Prost). Prost marks a service task unhealthy after three failed probes in a row (`fall = 3`) and takes it out for a while after 20
  failed forwards in five seconds; HAProxy queues instead.
* **Disk full**: `docker system df`; the volumes `pgdata`, `s3data` (when RustFS is in the stack), `prom_data`, `clamdb`, and `STATE_DIR/backups`.
* **The whole machine is lost**: a new node, the kit of the release, the same `swarm.env` and **`STATE_DIR` (the keys!)**, then the restore of `backup.md`: `prepare secrets`, `prepare configs`, `restore FILE`, `prepare db`,
  `migrate`, `deploy`, the mail (`blobs.tar.gz` into the RustFS volume, or your bucket), and `verify`.

## The webmail (`WEBMAIL_HOST`)

* **Turn it on**: set `WEBMAIL_HOST=webmail.example.org` in `swarm.env` (a DNS name pointing at the edge, like `ADMIN_HOST`), then `upgrade`. The new service needs its own database role: `upgrade` makes it (`prepare db` runs before `migrate`; a kit older
  than 2026-11-24 did not, and the webmail then fails to connect: run `prepare db` and `migrate` by hand once). The edge is HAProxy (or the Prost pilot beside it); the Prost edge does not route the webmail yet.
* **Sending**: the webmail only queues a message (`webmail_outbox`); the **submission** service sends it. If the submission service is down, messages wait in the Outbox ("Waiting") and go when it is back; a message that was refused shows a fixed
  reason (`not_yours`, `limit`, `refused`, `busy`). The sending budget (messages an hour) is the same as for SMTP submission.
* **Two-step sign-in and app passwords**: people turn them on in the webmail Settings. Mail programs of an account with the second factor use app passwords (up to ten). See `design-2fa.md` and `design-app-passwords.md`.
* **What to look at when it breaks**: `docker service ps rusty_webmail`, `logs webmail`; "cannot connect to Postgres" means the role (`prepare db`), "cannot read the Postgres schema state" means the grants (`migrate` after the role exists).

## The name server (`DNS_NAME_SERVERS`)

* **Turn it on**: `DNS_NAME_SERVERS="ns1.example.org ns2.example.org"` in `swarm.env` (and `DNS_HOSTMASTER`, by default `hostmaster.` plus the domain of the first name server), then `upgrade`. The service `dns` publishes port 53 (UDP and TCP) in host mode on every node
  labelled `dns=true` in the inventory (`NODES` labels), one task per node; with no such node, one task on the Manager. The names must exist as `A` records (glue) at the registrar of their own domain and point at those nodes.
* **Port 53 must be free on the node**: on Ubuntu `systemd-resolved` listens on 127.0.0.53:53 and a published port on 0.0.0.0 clashes with it; set `DNSStubListener=no` in `/etc/systemd/resolved.conf` (or bind the node to an address of its own) first.
* **A domain is hosted** by a platform administrator on its DNS page (Name servers of the platform); the tenant then sets the two names at its registrar and adds its own records there. A domain that is not hosted is not answered (`REFUSED`): the server is never recursive.
* **When it breaks**: `docker service ps rusty_dns`, `logs dns`. It reads the zones again every 15 seconds and keeps the last good ones when the database cannot be read (for a day by default, then it refuses everything). "cannot bind" means port 53 is taken.
