# Design: the basic monitor in the admin web

Status: built (the migration, the heartbeat task of every service, the API and the page). The owner asked for a basic monitor inside the admin page to watch the services (2026-11-16). It is a first look, not a metrics system: Prometheus metrics and alerts
come later (`roadmap.md`) and will not replace this page.

## What it shows

For a platform administrator (a domain administrator gets `403`):

* **Services:** every service (smtp-in, submission, imap, outbound, gc, admin) with its replicas: version, uptime, last seen, status. A service with no live replica is `down`; one with a replica
  that reports a problem, or that stopped answering while others live, is `degraded`.
* **Outbound queue:** queued, retrying and dead jobs, and the age of the oldest due one.
* **Storage:** every blob store and pool backend with its last check status and time.
* **DNS:** how many domains have a record in a bad state.

The page refreshes itself every 15 seconds. All text goes through the message catalogues (the source language is English).

## How the admin web knows (heartbeats)

The admin web must not reach into the other services over the network (they are on other networks and several replicas may run), and it must not read their data. Instead every service writes a
heartbeat row to PostgreSQL and the admin web reads the table.

`service_heartbeats (service, instance, version, started_at, last_seen, status, detail)`, key `(service, instance)`:

* `instance` is the container's host name, so every replica has its own row and scaling is visible.
* `last_seen` and `started_at` are written from the database clock (`rs_now()`), never from the service's clock.
* `status` is `ok` or `degraded`; `detail` is a small bounded JSON object of fixed keys (the listeners that are up; the scanners the inbound service reaches; the last collection pass of gc).
* A service writes every 15 seconds. A row not renewed for 45 seconds (three beats) is `stale`; `rusty-gc` deletes rows not seen for a day.

Writes go through one `SECURITY DEFINER` function, `rs_heartbeat(instance, version, started_at, status, detail)`. It derives the service name from `current_user` (`rusty_imap` is `imap`), so a
compromised service cannot write another service's row, and the roles hold `EXECUTE` on it and no write on the table. `rusty_admin` gets `SELECT` on the table. A heartbeat carries no mail data and
no secret.

## API

`GET /api/v1/monitor` (platform scope only) returns the four blocks above in one JSON document; the status of a service is computed on the server from the rows (the browser never decides what
"stale" means). Like every API response it has `Cache-Control: no-store`.

## Tests

* The store: an upsert renews a row and keeps `started_at` of the same process; a different instance is another row; the function refuses to be used to write for another service (a role-by-role
  test in `pg_service_roles.rs`); the cleanup removes only old rows.
* The service: the heartbeat task writes, survives a database error (it logs and tries again, it never takes the service down), and stops on shutdown.
* The API: shapes, `403` for a domain administrator, `down` / `degraded` / `ok` from crafted rows, staleness by the database clock.
* The page: loading, empty, a `down` service, a stale replica, refresh and the error note.

## Order of work

1. Migration, the function, the grants, the repository and its tests.
2. The heartbeat task in the services (every binary) and the cleanup in gc.
3. The API.
4. The page and the catalogue keys.
5. Docs and a run on the test server.
